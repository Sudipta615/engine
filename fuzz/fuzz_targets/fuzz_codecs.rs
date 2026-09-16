#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    let path = std::env::temp_dir().join(format!("libfuzzer_{}.bin", std::process::id()));
    if std::fs::write(&path, data).is_ok() {
        let _ = engine::decode::Decoder::open(&path);
        let _ = engine::decode::extract_loudness_metadata(&path);
        let _ = std::fs::remove_file(&path);
    }
});
