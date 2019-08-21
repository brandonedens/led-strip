#[macro_use]
extern crate log;

use chrono::{Datelike, Local, TimeZone, Utc};

use structopt::StructOpt;

use spidev::{SpiModeFlags, Spidev, SpidevOptions};
use std::io;
use std::io::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn create_spi() -> io::Result<Spidev> {
    let mut spi = Spidev::open("/dev/spidev0.0")?;
    let options = SpidevOptions::new()
        .bits_per_word(8)
        .max_speed_hz(15_000_000)
        .mode(SpiModeFlags::SPI_MODE_0)
        .build();
    spi.configure(&options)?;
    Ok(spi)
}

#[derive(Debug)]
#[repr(C)]
struct Color {
    flag: u8,
    blue: u8,
    green: u8,
    red: u8,
}

impl Color {
    fn new(red: u8, green: u8, blue: u8) -> Self {
        let mut flag = (red & 0xC0) >> 6;
        flag |= (green & 0xC0) >> 4;
        flag |= (blue & 0xC0) >> 2;
        flag = !flag;

        Color {
            flag,
            blue,
            green,
            red,
        }
    }
}

struct GammaTable {
    red_table: [u8; 256],
    green_table: [u8; 256],
    blue_table: [u8; 256],
}

impl GammaTable {
    fn new(red: f64, green: f64, blue: f64) -> Self {
        let mut gamma_table = GammaTable {
            red_table: [0u8; 256],
            green_table: [0u8; 256],
            blue_table: [0u8; 256],
        };
        for i in 0..256 {
            gamma_table.red_table[i] = (((i as f64 / 255_f64).powf(red)) * 255.0 + 0.5) as u8;
            gamma_table.green_table[i] = (((i as f64 / 255_f64).powf(blue)) * 255.0 + 0.5) as u8;
            gamma_table.blue_table[i] = (((i as f64 / 255_f64).powf(green)) * 255.0 + 0.5) as u8;
        }
        gamma_table
    }

    fn correct_color(&self, red: u8, green: u8, blue: u8) -> Color {
        Color::new(
            self.red_table[red as usize],
            self.green_table[green as usize],
            self.blue_table[blue as usize],
        )
    }
}

fn send_pixels(spi: &mut Spidev, pixels: &[Color]) -> io::Result<()> {
    let bytes: &[u8] = unsafe {
        ::std::slice::from_raw_parts(
            (pixels.as_ptr()) as *const u8,
            pixels.len() * ::std::mem::size_of::<Color>(),
        )
    };
    trace!("pixels: {:02x?}", pixels);
    trace!("bytes: {:02x?}", bytes);
    spi.write_all(bytes)?;
    Ok(())
}

fn hsv_to_rgb(hue: f64, saturation: f64, value: f64) -> (f64, f64, f64) {
    if saturation < 1.0e-6 {
        return (value, value, value);
    }

    let mut hue = hue;
    hue /= 60.0;

    let i = hue.floor();
    let frac = hue - i;
    let p = value * (1.0 - saturation);
    let q = value * (1.0 - saturation * frac);
    let t = value * (1.0 - saturation * (1.0 - frac));

    let color = match i as u8 {
        0 => (value, t, p),
        1 => (q, value, p),
        2 => (p, value, t),
        3 => (p, q, value),
        4 => (t, p, value),
        _ => (value, p, q),
    };

    (color.0, color.1, color.2)
}

fn hue_to_pixels(hue: &[f64], gamma_table: &GammaTable, gamma: f64) -> Vec<Color> {
    let mut pixels = hue
        .iter()
        .map(|h| {
            let (r, g, b) = hsv_to_rgb(*h, 1.0, 1.0);
            gamma_table.correct_color((r * gamma) as u8, (g * gamma) as u8, (b * gamma) as u8)
        })
        .collect::<Vec<Color>>();
    pixels.insert(
        0,
        Color {
            flag: 0,
            red: 0,
            green: 0,
            blue: 0,
        },
    );
    pixels.push(Color {
        flag: 0,
        red: 0,
        green: 0,
        blue: 0,
    });
    pixels.push(Color {
        flag: 0,
        red: 0,
        green: 0,
        blue: 0,
    });
    pixels
}

#[derive(Debug, StructOpt)]
#[structopt(name = "blink", about = "Control for TCL p9813 LED chip.")]
struct Opt {
    /// Latitude used for sunrise calculations.
    #[structopt(long = "latitude")]
    lat: f64,
    /// Longitude used for sunrise calculations.
    #[structopt(long = "longitude")]
    lon: f64,
}

fn main() {
    let opt = Opt::from_args();

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        r.store(false, Ordering::SeqCst);
    })
    .expect("Error setting Ctrl-C handler");

    let mut spi = create_spi().unwrap();

    const NUM_LEDS: usize = 76;
    let gamma_table = GammaTable::new(2.2, 2.2, 2.2);

    // Set starting color of all the pixels.
    let mut hue = [0f64; NUM_LEDS];
    hue.iter_mut().enumerate().for_each(|(i, v)| {
        *v = (i as f64 * 360f64) / NUM_LEDS as f64;
    });

    while running.load(Ordering::SeqCst) {
        let now = Local::now();
        let (sunrise, sunset) =
            sunrise::sunrise_sunset(opt.lat, opt.lon, now.year(), now.month(), now.day());
        let sunrise = Utc.timestamp(sunrise, 0);
        let sunset = Utc.timestamp(sunset, 0);

        let now = Utc::now();

        let mut gamma: f64 = 255.0;

        if now > sunrise && now < sunset {
            // Lights don't operate during the day.
            gamma = 0.0
        } else if now < sunrise {
            let delta = sunrise - now;
            const TWO_HOURS: f64 = (60 * 60 * 2) as f64;
            gamma = 255.0 - ((delta.num_seconds() as f64 * 255.0) / TWO_HOURS);
        } else if now > sunset {
            let delta = now - sunset;
            const THREE_HOURS: f64 = (60 * 60 * 3) as f64;
            gamma = 255.0 - ((delta.num_seconds() as f64 * 255.0) / THREE_HOURS);
        }

        hue.iter_mut().for_each(|v| {
            *v += 0.20;
            if *v >= 360.0 {
                *v = 0.0;
            }
        });

        let pixels = hue_to_pixels(&hue[..], &gamma_table, gamma);
        send_pixels(&mut spi, &pixels).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(16));
    }

    let pixels = hue_to_pixels(&hue[..], &gamma_table, 0.0);
    send_pixels(&mut spi, &pixels).unwrap();
}
