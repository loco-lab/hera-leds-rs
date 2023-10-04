use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use csv::ReaderBuilder;
use futures::{FutureExt, StreamExt};
use hifitime::Epoch;
use serde::{
    de::{self, Unexpected, Visitor},
    Deserializer,
};
use smart_leds::{SmartLedsWrite, RGB8};
use tokio::{
    signal,
    sync::mpsc::{channel, Receiver, Sender},
};
use tokio_stream::wrappers::IntervalStream;
use tokio_util::{bytes::Buf, sync::CancellationToken};
use ws281x_rpi::Ws2812Rpi;

#[derive(Debug, serde::Deserialize)]
enum Polarization {
    #[serde(alias = "e")]
    E,
    #[serde(alias = "n")]
    N,
}

/// Converts a string to a boolean based on truthy and falsy values.
///
/// Designed to be used as #[serde(deserialize_with = "bool_from_str")]
fn bool_from_str<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    struct BoolVisitor;
    impl Visitor<'_> for BoolVisitor {
        type Value = bool;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            write!(
                formatter,
                "truthy (t, true, 1, on, y, yes) or falsey (f, false, 0, off, n, no) string"
            )
        }
        fn visit_str<E: de::Error>(self, s: &str) -> Result<bool, E> {
            match s {
                "t" | "T" | "true" | "True" | "1" | "on" | "On" | "y" | "Y" | "yes" | "Yes" => {
                    Ok(true)
                }
                "f" | "F" | "false" | "False" | "0" | "off" | "Off" | "n" | "N" | "no" | "No" => {
                    Ok(false)
                }
                other => {
                    // handle weird mixed-case spellings like tRue or nO
                    match other.to_lowercase().as_str() {
                        "true" | "on" | "yes" => Ok(true),
                        "false" | "off" | "no" => Ok(false),
                        other => Err(de::Error::invalid_value(Unexpected::Str(other), &self)),
                    }
                }
            }
        }
    }

    deserializer.deserialize_str(BoolVisitor)
}

#[derive(Debug, serde::Deserialize)]
#[allow(unused)]
/// Columns of the ant_stats csv downloaded from HERAnow
/// some columns can be "Unknown" so use the invalid_option parser
struct HeraAuto {
    ant: u16,
    pol: Polarization,
    #[serde(deserialize_with = "bool_from_str")]
    constructed: bool,
    #[serde(deserialize_with = "csv::invalid_option")]
    node: Option<u8>,
    #[serde(deserialize_with = "csv::invalid_option")]
    fem_switch: Option<u8>,
    apriori: String,
    spectra: Option<f64>,
    pam_power: Option<f64>,
    adc_power: Option<f64>,
    adc_rms: Option<f64>,
    fem_imu_theta: Option<f64>,
    fem_imu_phi: Option<f64>,
    eq_coeffs: Option<f64>,
}

// A dummy enum to merge streams together
enum StreamResult<T> {
    Proceed(T),
    Cancel,
}

/// Calculate the RGB values given a magnitude, min and max
fn colorscale(mag: f64, cmin: f64, cmax: f64) -> RGB8 {
    let x = {
        let mut tmp = (mag - cmin) / (cmax - cmin);
        if !tmp.is_finite() {
            tmp = 0.5;
        }
        tmp
    };
    RGB8::new((x * 255.0) as u8, 255, (75.0 - (x * 75.0)) as u8)
}

#[derive(Debug, Default, Clone, Copy)]
/// Spectra used to compute color of the LED
/// Created by merging the HeraAuto from both E and N pols
struct LedStat {
    #[allow(unused)]
    ant: u16,
    constructed: bool,
    spectra: Option<f64>,
}
impl LedStat {
    /// Convert the spectra to a color in our colorscale
    /// if the antenna is not constructed turn it off
    /// if the spectra is out of bounds, make it red.
    fn get_color(&self) -> RGB8 {
        match self.constructed {
            false => RGB8::new(0, 0, 0),
            true => match self.spectra {
                None => RGB8::new(0, 127, 255),
                Some(auto) => match (-45.0..=-20.0).contains(&auto) {
                    true => colorscale(auto, -100.0, -10.0),
                    false => RGB8::new(255, 50, 0),
                },
            },
        }
    }
}

/// Once a minute, proble heranow for the ant_stats.csv
/// Culminate all the antennas with ant < 320 int an array of [LedStat]
/// then send these to to the worker thread
async fn probe_online_stats(
    cancel_token: &CancellationToken,
    stat_tx: Sender<[LedStat; 320]>,
) -> Result<()> {
    let cancel_indication = cancel_token.cancelled();
    tokio::pin!(cancel_indication);

    let interval = IntervalStream::new({
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    })
    .map(StreamResult::Proceed);
    let cancel_stream = cancel_indication
        .into_stream()
        .map(|_| StreamResult::Cancel);

    let mut loop_stream = tokio_stream::StreamExt::merge(interval, cancel_stream);

    let stats_init: [LedStat; 320] = (0..320)
        .map(|x| LedStat {
            ant: x,
            constructed: false,
            spectra: Some(f64::NEG_INFINITY),
        })
        .collect::<Vec<_>>()
        .try_into()
        .unwrap();

    while let Some(StreamResult::Proceed(_tick)) = loop_stream.next().await {
        let mut stats = stats_init;
        let resp = reqwest::get("https://heranow.reionization.org/media/ant_stats.csv").await?;
        if resp.status().is_success() {
            let mut reader = ReaderBuilder::new()
                .delimiter(b',')
                .has_headers(true)
                .trim(csv::Trim::Fields)
                .from_reader(resp.bytes().await?.reader());

            reader
                .deserialize::<HeraAuto>()
                .flatten()
                .filter(|auto| auto.ant < 320)
                .for_each(|record| {
                    stats[record.ant as usize].constructed |= record.constructed;
                    stats[record.ant as usize].spectra =
                        match (stats[record.ant as usize].spectra, record.spectra) {
                            (None, None) => None,
                            (None, Some(_)) => None,
                            (Some(_), None) => None,
                            (Some(x), Some(y)) => {
                                if !x.is_finite() {
                                    Some(y)
                                } else if !y.is_finite() || !(-45.0..=-20.0).contains(&x) {
                                    // if either of the dipoles are out of range
                                    // use that value. then we'll set to red later
                                    Some(x)
                                } else if !(-45.0..=-20.0).contains(&y) {
                                    Some(y)
                                } else {
                                    Some((x + y) / 2.0)
                                }
                            }
                        };
                });

            stat_tx.send(stats).await?;
        }
    }

    Ok(())
}

/// Listend for an interrupt signal.
pub async fn interrupt(
    token: CancellationToken,
    _sender: Sender<()>,
) -> Result<(), std::io::Error> {
    let cancel = token.cancelled();
    tokio::pin!(cancel);
    let token_stream = cancel.into_stream().map(|_| StreamResult::Cancel);

    let signal_future = signal::ctrl_c();
    tokio::pin!(signal_future);
    let signal_stream = signal_future.into_stream().map(StreamResult::Proceed);

    let mut loop_stream = tokio_stream::StreamExt::merge(signal_stream, token_stream);

    if let Some(StreamResult::Proceed(sig)) = loop_stream.next().await {
        token.cancel();
        sig?;
    }
    Ok(())
}

// The entry is the light corresponding to the index's second.
// e.g. second 3 of every minute should illuminate LED 211
static SECONDS: [usize; 60] = [
    214, 213, 212, 211, 210, 209, 208, 207, 206, 205, 204, 203, 202, 201, 200, 199, 180, 179, 160,
    159, 140, 139, 120, 119, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 32, 33, 54, 55, 76, 77, 98, 99,
    309, 308, 287, 286, 265, 264, 243, 242, 221, 220, 219, 218, 217, 216, 215, 215,
];

// LED numbers of clock hands.
// Mappings of hours to the leds necessay to make that hand on a clock
static HOUR01: &[usize] = &[319, 298, 297, 276, 275, 254, 253, 232, 231];
static HOUR02: &[usize] = &[110, 131, 152, 173, 194];
static HOUR03: &[usize] = &[110, 128, 132, 146, 154, 164, 176, 182, 198];
static HOUR04: &[usize] = &[110, 127, 134, 143, 158];
static HOUR05: &[usize] = &[110, 111, 112, 113, 114, 115, 116, 117, 118];
static HOUR06: &[usize] = &[109, 86, 63, 40, 17];
static HOUR07: &[usize] = &[109, 89, 85, 69, 61, 49, 37, 29, 13];
static HOUR08: &[usize] = &[109, 90, 83, 72, 57];
static HOUR09: &[usize] = &[109, 108, 107, 106, 105, 104, 103, 102, 101, 100];
static HOUR10: &[usize] = &[319, 300, 293, 282, 267];
static HOUR11: &[usize] = &[319, 299, 295, 279, 271, 259, 247, 239, 223];
static HOUR12: &[usize] = &[319, 296, 273, 250, 227];
static HOUR_HANDS: &[&[usize]] = &[
    HOUR01, HOUR02, HOUR03, HOUR04, HOUR05, HOUR06, HOUR07, HOUR08, HOUR09, HOUR10, HOUR11, HOUR12,
];

/// Correlates the antenna number defined by HERA to the LED number in the sequential string.
/// Antenna is the index and the LED number is the entry
static ANTENNAS: &[usize] = &[
    10, 9, 8, 7, 6, 5, 4, 3, 2, 1, 0, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 119, 32, 31, 30,
    29, 28, 27, 26, 25, 24, 23, 22, 118, 120, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 117, 121,
    139, 54, 53, 52, 51, 50, 49, 48, 47, 46, 45, 44, 116, 122, 138, 140, 55, 56, 57, 58, 59, 60,
    61, 62, 63, 64, 65, 115, 123, 137, 141, 159, 76, 75, 74, 73, 72, 71, 70, 69, 68, 67, 66, 114,
    124, 136, 142, 158, 160, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 113, 125, 135, 143, 157,
    161, 179, 98, 97, 96, 95, 94, 93, 92, 91, 90, 89, 88, 112, 126, 134, 144, 156, 162, 178, 180,
    99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 111, 127, 133, 145, 155, 163, 177, 181,
    199, 309, 310, 311, 312, 313, 314, 315, 316, 317, 318, 319, 110, 128, 132, 146, 154, 164, 176,
    182, 198, 200, 308, 307, 306, 305, 304, 303, 302, 301, 300, 299, 298, 129, 131, 147, 153, 165,
    175, 183, 197, 201, 287, 288, 289, 290, 291, 292, 293, 294, 295, 296, 297, 130, 148, 152, 166,
    174, 184, 196, 202, 286, 285, 284, 283, 282, 281, 280, 279, 278, 277, 276, 149, 151, 167, 173,
    185, 195, 203, 265, 266, 267, 268, 269, 270, 271, 272, 273, 274, 275, 150, 168, 172, 186, 194,
    204, 264, 263, 262, 261, 260, 259, 258, 257, 256, 255, 254, 169, 171, 187, 193, 205, 243, 244,
    245, 246, 247, 248, 249, 250, 251, 252, 253, 170, 188, 192, 206, 242, 241, 240, 239, 238, 237,
    236, 235, 234, 233, 232, 189, 191, 207, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231,
    190, 208, 220, 219, 218, 217, 216, 215, 214, 213, 212, 211, 210, 209,
];

const PIN: i32 = 18;
const NUM_LEDS: usize = 320;

/// Print the current time on a static background
async fn write_time(cancel_token: &CancellationToken, _sender: Sender<()>) -> Result<()> {
    let data: [RGB8; NUM_LEDS] = [RGB8::new(0, 0, 0); NUM_LEDS];
    // // draw all the things
    // data.iter_mut()
    //     .for_each(|pix| *pix = colorscale(-80.0, -100.0, -10.0));
    print!("making light strip...");
    let mut ws = Ws2812Rpi::new(NUM_LEDS as i32, PIN).unwrap();
    println!("Done.");
    let cancel_indication = cancel_token.cancelled();
    tokio::pin!(cancel_indication);

    let interval = IntervalStream::new({
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    })
    .map(StreamResult::Proceed);
    let cancel_stream = cancel_indication
        .into_stream()
        .map(|_| StreamResult::Cancel);

    let mut loop_stream = tokio_stream::StreamExt::merge(interval, cancel_stream);
    println!("Start ticking.");
    while let Some(StreamResult::Proceed(_tick)) = loop_stream.next().await {
        // set the current second to white
        let now = Epoch::now().unwrap();

        let (_year, _month, _day, hour, min, sec, _) = now.to_gregorian_utc();

        let mut data_to_write = data;
        // set the second light
        data_to_write[SECONDS[sec as usize]] = RGB8::new(200, 200, 200);
        // set the minute light
        data_to_write[SECONDS[min as usize]] = RGB8::new(125, 125, 125);
        // set the hour hand
        // hour is given in 24 hour clock
        for indices in HOUR_HANDS[(hour % 12) as usize] {
            data_to_write[*indices] = RGB8::new(50, 50, 50);
        }

        ws.write(data_to_write.into_iter()).unwrap();
    }
    Ok(())
}

/// Print the autospectra with the seconds hand moving around.
async fn draw_autos(
    cancel_token: &CancellationToken,
    mut stat_rx: Receiver<[LedStat; 320]>,
    _sender: Sender<()>,
) -> Result<()> {
    let mut data: [RGB8; NUM_LEDS] = [RGB8::new(0, 0, 0); NUM_LEDS];
    let mut ws = Ws2812Rpi::new(NUM_LEDS as i32, PIN).unwrap();

    let mut interval = {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval
    };

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // data impls Copy so we don't have to worry about over writing
                let mut data_to_write = data;
                // set the current second to white
                let now = Epoch::now().unwrap();
                let (_year, _month, _day, _hour, _min, sec, _) = now.to_gregorian_utc();

                // set the second light
                data_to_write[SECONDS[sec as usize]] = RGB8::new(200, 200, 200);

                ws.write(data_to_write.iter().cloned())?;

            },
            Some(new_stats) = stat_rx.recv() => {
                println!("new stats loaded");
                new_stats.iter().for_each(|stat| data[ANTENNAS[stat.ant as usize]] = stat.get_color());
                // data impls Copy so we don't have to worry about over writing
                let mut data_to_write = data;
                // set the current second to white
                let now = Epoch::now().unwrap();
                let (_year, _month, _day, _hour, _min, sec, _) = now.to_gregorian_utc();

                // set the second light
                data_to_write[SECONDS[sec as usize]] = RGB8::new(200, 200, 200);

                ws.write(data_to_write.iter().cloned())?;
            }
            _ = cancel_token.cancelled() => break,
            else =>  break,
        }
    }
    Ok(())
}

#[derive(Parser)]
struct Cli {
    #[clap(short, required = false, default_value_t = false)]
    time: bool,
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();

    let (send, mut recv) = channel::<()>(1);

    // initialize a task to listen for an interrupt signal
    let cancel_token = CancellationToken::new();

    let interrupt_send = send.clone();
    let interrupt_token = cancel_token.clone();
    tokio::spawn(async move {
        if let Err(err) = interrupt(interrupt_token, interrupt_send).await {
            println!("Error during signal listener process: {err}")
        }
    });

    print!("stating up local task...");
    // Construct a local task set that can run `!Send` futures.
    let local = tokio::task::LocalSet::new();
    println!("Done.");
    if args.time {
        println!("Starting clock.");
        local.spawn_local(async move {
            println!("inside local.");
            if let Err(err) = write_time(&cancel_token, send).await {
                println!("Error during clock display: {err:?}");
                cancel_token.cancel();
            };
        });
    } else {
        let (stat_tx, stat_rx) = channel(1);

        let http_token = cancel_token.child_token();
        tokio::spawn(async move {
            if let Err(err) = probe_online_stats(&http_token, stat_tx).await {
                println!("Error during online stat generation: {err:?}. Shutting down.");
                http_token.cancel();
            }
        });

        local.spawn_local(async move {
            if let Err(err) = draw_autos(&cancel_token, stat_rx, send).await {
                println!("Error during auto spectrum printing: {err:?}. Shutting down.");
                cancel_token.cancel();
            };
        });
    }
    local.await;

    let _ = recv.recv().await;

    println!("Exiting");
}
