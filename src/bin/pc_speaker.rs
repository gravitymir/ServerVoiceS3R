//! PC speaker server: capture Windows system audio (WASAPI loopback) and stream
//! it to the ATOM VoiceS3R as 16 kHz mono s16le PCM over TCP (default port 9001).
//!
//! Run it, then put the device into "speaker mode" (voice command). The device
//! connects here and plays whatever your PC is outputting.
//!
//!   cargo run --release --bin pc_speaker

use std::collections::VecDeque;
use std::io::Write;
use std::net::TcpListener;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const DEVICE_RATE: u32 = 16_000;

fn main() {
    let port: u16 = std::env::var("SPEAKER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9001);

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("no default output device");
    println!(
        "[speaker] loopback-capturing: {}",
        device.name().unwrap_or_default()
    );
    let cfg = device
        .default_output_config()
        .expect("no default output config");
    let src_rate = cfg.sample_rate().0;
    let channels = cfg.channels() as usize;
    println!(
        "[speaker] source {} Hz, {} ch, {:?} -> 16 kHz mono",
        src_rate,
        channels,
        cfg.sample_format()
    );

    // Resampled mono s16le bytes, produced by the audio callback, drained by TCP.
    let buf = Arc::new(Mutex::new(VecDeque::<u8>::new()));
    let ratio = src_rate as f32 / DEVICE_RATE as f32; // src samples per output sample
    let cap = (DEVICE_RATE as usize) * 2; // ~0.5 s of mono bytes -> bounds latency

    let buf_cb = buf.clone();
    let mut phase = 0f32;
    let err_fn = |e| eprintln!("[speaker] stream error: {e}");

    // Self-diagnostic: log the capture level every 2s (so we can verify loopback
    // works without an external test client).
    let peak = Arc::new(AtomicI32::new(0));
    let peak_cb = peak.clone();
    {
        let peak_log = peak.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            let p = peak_log.swap(0, Ordering::Relaxed);
            println!("[speaker] capture level peak={p}/32767");
        });
    }

    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => device
            .build_input_stream(
                &cfg.clone().into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let frames = data.len() / channels;
                    let mut out = buf_cb.lock().unwrap();
                    let mut i = phase;
                    while (i as usize) < frames {
                        let base = i as usize * channels;
                        let mut s = 0f32;
                        for c in 0..channels {
                            s += data[base + c];
                        }
                        s /= channels as f32;
                        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                        peak_cb.fetch_max((v as i32).abs(), Ordering::Relaxed);
                        out.extend(v.to_le_bytes());
                        i += ratio;
                    }
                    phase = i - frames as f32;
                    while out.len() > cap {
                        out.pop_front();
                    }
                },
                err_fn,
                None,
            )
            .expect("failed to open loopback stream"),
        other => panic!("unsupported sample format {other:?} (expected F32)"),
    };
    stream.play().expect("failed to start capture");

    let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind");
    println!("[speaker] streaming on 0.0.0.0:{port} — say \"speaker mode\" to the device");

    for client in listener.incoming() {
        let mut c = match client {
            Ok(c) => c,
            Err(_) => continue,
        };
        c.set_nodelay(true).ok();
        let peer = c.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        println!("[speaker] device connected: {peer}");
        // Clear any backlog so we start near-live.
        buf.lock().unwrap().clear();
        loop {
            let chunk: Vec<u8> = {
                let mut b = buf.lock().unwrap();
                let n = b.len().min(2048);
                b.drain(..n).collect()
            };
            if chunk.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            if c.write_all(&chunk).is_err() {
                break;
            }
        }
        println!("[speaker] device disconnected: {peer}");
    }
}
