#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // 1. CUE parser
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = engine::decode::cue::CueSheet::parse(s);
        let _ = engine::spatial::adm::parse_adm_xml(s);
    }

    // 2. SOFA parser
    let _ = engine::spatial::sofa::import_sofa(data, None);

    // 3. Graph2 JSON deserializer
    let _ = serde_json::from_slice::<engine::dsp::graph2::Graph2>(data);

    // 4. Spatial scene JSON deserializer
    let _ = serde_json::from_slice::<config::SpatialSceneConfig>(data);
});
