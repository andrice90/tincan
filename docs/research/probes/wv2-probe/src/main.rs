// Throwaway probe: WebCodecs support matrix inside WebView2 (via Tauri 2) and Channel<Raw> timing.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::ipc::{Channel, InvokeResponseBody};

#[tauri::command]
fn stream(count: u32, interval_us: u64, size: usize, on_data: Channel<InvokeResponseBody>) {
    std::thread::spawn(move || {
        let start = Instant::now();
        for seq in 0..count {
            let due = start + Duration::from_micros(interval_us * seq as u64);
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            let mut buf = vec![0u8; size.max(16)];
            let us = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros() as u64;
            buf[0..8].copy_from_slice(&us.to_le_bytes());
            buf[8..12].copy_from_slice(&seq.to_le_bytes());
            if on_data.send(InvokeResponseBody::Raw(buf)).is_err() {
                break;
            }
        }
    });
}

#[tauri::command]
fn replay_path() -> Option<String> {
    std::env::var("REPLAY_FILE").ok()
}

#[tauri::command]
fn replay(path: String, on_data: Channel<InvokeResponseBody>) {
    std::thread::spawn(move || {
        let data = std::fs::read(&path).unwrap();
        let mut i = 0usize;
        let start = Instant::now();
        let mut n = 0u64;
        while i + 13 <= data.len() {
            let len = u32::from_le_bytes(data[i..i + 4].try_into().unwrap()) as usize;
            let key = data[i + 4];
            let ts = &data[i + 5..i + 13];
            let payload = &data[i + 13..i + 13 + len];
            i += 13 + len;
            let due = start + Duration::from_micros(33_333 * n);
            n += 1;
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            let mut msg = Vec::with_capacity(9 + len);
            msg.push(key);
            msg.extend_from_slice(ts);
            msg.extend_from_slice(payload);
            let _ = on_data.send(InvokeResponseBody::Raw(msg));
        }
        let _ = on_data.send(InvokeResponseBody::Raw(vec![0xFF]));
    });
}

#[tauri::command]
fn report(app: tauri::AppHandle, data: String) {
    let path = std::env::var("PROBE_OUT").unwrap_or_else(|_| "probe-result.json".into());
    std::fs::write(&path, data).unwrap();
    app.exit(0);
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![stream, report, replay, replay_path])
        .run(tauri::generate_context!())
        .expect("run");
}
