//! Stage 3 Fidelity Suite: Spatial Audio & Professional Interchange.
//!
//! Validates all 8 pillars of Stage 3 (§4.1–§4.7, §10.5):
//! - Pillar 1: Formal spatial representations (§4.3, Item 21)
//! - Pillar 2: Physical vs spatial channel separation (§4.4, Item 22)
//! - Pillar 3: ADM data model and XML codec (§4.1, Item 23)
//! - Pillar 4: BWF/BW64 container interoperability (§4.2, Item 24)
//! - Pillar 5: Object metadata expansion (§4.5, Item 25)
//! - Pillar 6: HRTF profile architecture (§4.6, Item 26)
//! - Pillar 7: Spatial quality evaluation framework (§4.7, Item 27)
//! - Pillar 8: Output-profile calibration (§10.5, Item 28)

use std::io::Cursor;

use engine::output::{OutputCalibration, OutputProfile, TargetLayoutKind};
use engine::spatial::adm::{parse_adm_xml, to_adm_xml, AdmSceneConverter};
use engine::spatial::bw64::{
    BextChunk, Bw64ContainerType, Bw64File, Bw64Metadata, ChnaChunk, ChnaTrackUid,
};
use engine::spatial::channels::{PhysicalChannelCount, SpatialFieldOrder};
use engine::spatial::hrtf::{
    HrtfDataset, HrtfInterpolationMethod, HrtfProfile, HrtfProfileManager,
};
use engine::spatial::math::Vec3;
use engine::spatial::object::{ObjectAudioRef, ScreenReference, SpatialAudioObject, ZoneExclusion};
use engine::spatial::panner::BasicPanner;
use engine::spatial::quality_eval::SpatialQualityEvaluator;
use engine::spatial::representation::{
    RepresentationConversion, SpatialInputDescriptor, SpatialRepresentation,
    SpatialRepresentationKind,
};
use engine::spatial::speaker::SpeakerLayout;
use engine::spatial::{BinauralBuffer, HoaBuffer, PhysicalBuffer, SpatialScene};
use engine::standards::adm::AdmStandard;

// ─────────────────────────────────────────────────────────────────────────────
// Pillar 1: Formal Spatial Representations (§4.3, Item 21)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pillar1_formal_spatial_representations() {
    let stereo_layout = SpeakerLayout::stereo();
    let ch_rep = SpatialRepresentation::channel_based(&stereo_layout);
    assert_eq!(ch_rep.kind(), SpatialRepresentationKind::ChannelBased);
    assert_eq!(ch_rep.channel_count(), 2);
    assert!(!ch_rep.is_binaural());

    let obj_rep = SpatialRepresentation::object_based(16, 64);
    assert_eq!(obj_rep.kind(), SpatialRepresentationKind::ObjectBased);
    assert_eq!(obj_rep.channel_count(), 16);

    let hoa3 = SpatialRepresentation::hoa_acn_sn3d(3);
    assert_eq!(hoa3.kind(), SpatialRepresentationKind::Hoa);
    assert_eq!(hoa3.channel_count(), 16); // (3+1)^2
    assert!(hoa3.is_soundfield());

    let hoa9 = SpatialRepresentation::hoa_acn_sn3d(9);
    assert_eq!(hoa9.channel_count(), 100); // (9+1)^2

    let bin_rep = SpatialRepresentation::binaural();
    assert_eq!(bin_rep.kind(), SpatialRepresentationKind::Binaural);
    assert_eq!(bin_rep.channel_count(), 2);
    assert!(bin_rep.is_binaural());

    let hybrid = SpatialRepresentation::hybrid(8, 2, 1);
    assert_eq!(hybrid.kind(), SpatialRepresentationKind::Hybrid);
    assert_eq!(hybrid.channel_count(), 11);

    // Conversions
    assert_eq!(
        RepresentationConversion::between(&obj_rep, &hoa3),
        Some(RepresentationConversion::ObjectToHoa)
    );
    assert_eq!(
        RepresentationConversion::between(&hoa3, &ch_rep),
        Some(RepresentationConversion::HoaToChannel)
    );
    assert_eq!(
        RepresentationConversion::between(&ch_rep, &bin_rep),
        Some(RepresentationConversion::ChannelToBinaural)
    );

    let desc = SpatialInputDescriptor::new(hoa3, 48000, 256);
    assert_eq!(
        desc.conversion_to(&ch_rep),
        Some(RepresentationConversion::HoaToChannel)
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Pillar 2: Physical vs Spatial Channel Separation (§4.4, Item 22)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pillar2_physical_vs_spatial_channel_separation() {
    let phys2 = PhysicalChannelCount::STEREO;
    assert_eq!(phys2.get(), 2);
    let phys16 = PhysicalChannelCount::IMMERSIVE_9_1_6;
    assert_eq!(phys16.get(), 16);

    let order9 = SpatialFieldOrder::ORDER_9;
    assert_eq!(order9.order(), 9);
    assert_eq!(order9.channels(), 100);

    // HOA order 9 buffer with 100 channels operates independently of physical limits
    let mut hoa_buf = HoaBuffer::new(order9, 128);
    assert_eq!(hoa_buf.channels(), 100);
    assert_eq!(hoa_buf.frames(), 128);

    if let Some(ch50) = hoa_buf.channel_mut(50) {
        ch50[0] = 2.0;
    }
    assert_eq!(hoa_buf.channel(50).unwrap()[0], 2.0);

    hoa_buf.scale(0.5);
    assert_eq!(hoa_buf.channel(50).unwrap()[0], 1.0);

    let hoa_copy = hoa_buf.clone();
    hoa_buf.accumulate(&hoa_copy);
    assert_eq!(hoa_buf.channel(50).unwrap()[0], 2.0);

    hoa_buf.clear();
    assert_eq!(hoa_buf.channel(50).unwrap()[0], 0.0);

    // Physical buffer remains compact
    let mut phys_buf = PhysicalBuffer::new(phys2, 128);
    assert_eq!(phys_buf.channels(), 2);
    phys_buf.clear();
    assert_eq!(phys_buf.as_slice().len(), 256);

    // Binaural buffer
    let mut bin = BinauralBuffer::new(2);
    bin.left[0] = 0.7;
    bin.right[0] = -0.7;
    let mut out_interleaved = [0.0f32; 4];
    bin.interleave_into(&mut out_interleaved);
    assert_eq!(out_interleaved[0], 0.7);
    assert_eq!(out_interleaved[1], -0.7);
}

// ─────────────────────────────────────────────────────────────────────────────
// Pillar 3: ADM Data Model & XML Codec (§4.1, Item 23)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pillar3_adm_data_model_and_xml_roundtrip() {
    let mut scene = SpatialScene::new(48000);
    let obj_id = scene
        .create_audio_object(Vec3::new(1.5, 2.5, 0.75))
        .unwrap();

    if let Some(obj) = scene.object_mut(obj_id) {
        obj.gain = 0.9;
        obj.spread = 0.4;
        obj.divergence = 0.6;
        obj.diffuseness = 0.15;
        obj.head_locked = true;
    }

    let doc = AdmSceneConverter::from_spatial_scene(&scene, AdmStandard::ItuBs2076_2);
    assert_eq!(doc.objects.len(), 1);
    assert!(doc.objects[0].head_locked);

    let xml = to_adm_xml(&doc);
    assert!(xml.contains("headLocked=\"true\""));
    assert!(xml.contains("<gain>0.9000</gain>"));

    let parsed_doc = parse_adm_xml(&xml).expect("XML parse failed");
    assert_eq!(parsed_doc.objects.len(), 1);
    assert_eq!(parsed_doc.pack_formats.len(), 1);
    assert_eq!(parsed_doc.channel_formats.len(), 1);

    let roundtrip_scene =
        AdmSceneConverter::to_spatial_scene(&parsed_doc).expect("to scene failed");
    assert_eq!(roundtrip_scene.objects.len(), 1);
    let r_obj = roundtrip_scene.objects.iter().next().unwrap();
    assert!(r_obj.head_locked);
    assert!((r_obj.gain - 0.9).abs() < 1e-3);
    assert!((r_obj.spread - 0.4).abs() < 1e-3);
    assert!((r_obj.divergence - 0.6).abs() < 1e-3);
    assert!((r_obj.diffuseness - 0.15).abs() < 1e-3);
}

// ─────────────────────────────────────────────────────────────────────────────
// Pillar 4: BWF/BW64 Container Interoperability (§4.2, Item 24)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pillar4_bwf_bw64_interoperability() {
    let metadata = Bw64Metadata {
        bext: Some(BextChunk {
            description: "Master 7.1.4 Immersive Mix".to_string(),
            originator: "Shadow Audio Engine".to_string(),
            loudness_value_lufs: Some(-24.0),
            loudness_range_lu: Some(12.0),
            max_true_peak_dbtp: Some(-1.5),
            max_momentary_lufs: Some(-18.0),
            max_short_term_lufs: Some(-21.0),
            ..Default::default()
        }),
        chna: Some(ChnaChunk {
            entries: vec![
                ChnaTrackUid {
                    track_index: 1,
                    uid: "ATU_00000001".to_string(),
                    track_format_id: "AT_00010001_01".to_string(),
                    pack_format_id: "AP_00010001".to_string(),
                },
                ChnaTrackUid {
                    track_index: 2,
                    uid: "ATU_00000002".to_string(),
                    track_format_id: "AT_00010002_01".to_string(),
                    pack_format_id: "AP_00010001".to_string(),
                },
            ],
        }),
        axml: Some("<ituBS2076><coreMetadata/></ituBS2076>".to_string()),
        ixml: Some("<BWFXML><TAKE>1</TAKE></BWFXML>".to_string()),
    };

    let file = Bw64File {
        container_type: Bw64ContainerType::Bw64,
        channels: 2,
        sample_rate: 96000,
        bits_per_sample: 24,
        metadata,
        audio_data_len: 24,
    };

    let audio_payload = vec![0xAA; 24];
    let mut cursor = Cursor::new(Vec::new());
    file.write_to(&mut cursor, &audio_payload)
        .expect("write failed");

    cursor.set_position(0);
    let parsed = Bw64File::parse(&mut cursor).expect("parse failed");

    assert_eq!(parsed.container_type, Bw64ContainerType::Bw64);
    assert_eq!(parsed.channels, 2);
    assert_eq!(parsed.sample_rate, 96000);
    assert_eq!(parsed.bits_per_sample, 24);

    let bext = parsed.metadata.bext.unwrap();
    assert_eq!(bext.description, "Master 7.1.4 Immersive Mix");
    assert_eq!(bext.loudness_value_lufs, Some(-24.0));
    assert_eq!(bext.max_true_peak_dbtp, Some(-1.5));

    let chna = parsed.metadata.chna.unwrap();
    assert_eq!(chnna_count(&chna), 2);
    assert_eq!(chna.entries[0].uid, "ATU_00000001");
    assert_eq!(chna.entries[1].uid, "ATU_00000002");

    assert!(parsed.metadata.axml.is_some());
    assert!(parsed.metadata.ixml.is_some());
}

fn chnna_count(chna: &ChnaChunk) -> usize {
    chna.entries.len()
}

// ─────────────────────────────────────────────────────────────────────────────
// Pillar 5: Object Metadata Expansion (§4.5, Item 25)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pillar5_object_metadata_expansion() {
    let obj = SpatialAudioObject::new(
        engine::spatial::object::ObjectId(1),
        ObjectAudioRef::None,
        Vec3::new(0.0, 2.0, 0.0),
    )
    .with_head_locked(true)
    .with_screen_relative(
        true,
        Some(ScreenReference {
            screen_id: "main_cinema_screen".to_string(),
            aspect_ratio: 2.39,
            width_m: 10.0,
            height_m: 4.18,
        }),
    )
    .with_divergence(0.75)
    .with_extent(2.0, 1.5, 0.5)
    .with_diffuseness(0.4)
    .with_absolute_distance(5.0)
    .with_zone_exclusion(ZoneExclusion {
        min_x: -1.0,
        max_x: 1.0,
        min_y: -1.0,
        max_y: 1.0,
        min_z: -0.5,
        max_z: 0.5,
    });

    assert!(obj.head_locked);
    assert!(obj.screen_relative);
    assert_eq!(obj.divergence, 0.75);
    assert_eq!(obj.extent.width, 2.0);
    assert_eq!(obj.extent.height, 1.5);
    assert_eq!(obj.extent.depth, 0.5);
    assert_eq!(obj.diffuseness, 0.4);
    assert_eq!(obj.absolute_distance, Some(5.0));
    assert_eq!(obj.distance_from_origin(Vec3::new(10.0, 0.0, 0.0)), 5.0);

    let screen = obj.screen_ref.unwrap();
    assert_eq!(screen.screen_id, "main_cinema_screen");
    assert_eq!(screen.aspect_ratio, 2.39);
}

// ─────────────────────────────────────────────────────────────────────────────
// Pillar 6: HRTF Profile Architecture (§4.6, Item 26)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pillar6_hrtf_profile_architecture() {
    let profile = HrtfProfile::kemar_reference(48000);
    assert_eq!(profile.id, "kemar_reference");
    assert_eq!(profile.sampling_rate, 48000);
    assert_eq!(
        profile.interpolation_method,
        HrtfInterpolationMethod::BarycentricTriangulation
    );

    let json = serde_json::to_string(&profile).expect("serialize profile failed");
    let loaded: HrtfProfile = serde_json::from_str(&json).expect("deserialize profile failed");
    assert_eq!(profile, loaded);

    let mut mgr = HrtfProfileManager::new();
    assert_eq!(mgr.active_profile().unwrap().id, "kemar_reference");

    let sphere_p = HrtfProfile::spherical_head_model(48000);
    mgr.register_profile(sphere_p);
    assert!(mgr.set_active_profile("spherical_model"));
    assert_eq!(mgr.active_profile().unwrap().id, "spherical_model");

    let dataset = HrtfDataset::synthetic(48000, 64, 15.0, 15.0);
    let metrics = profile.compute_quality_metrics(&dataset);
    assert!(metrics.mean_itd_us > 0.0);
    assert!(metrics.smoothness_index > 0.0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Pillar 7: Spatial Quality Evaluation Framework (§4.7, Item 27)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pillar7_spatial_quality_evaluation() {
    let mut panner = BasicPanner::new(10.0);
    let layout = SpeakerLayout::stereo();
    let report = SpatialQualityEvaluator::evaluate_panning(&mut panner, &layout, 48000);

    let rendered = report.render_report();
    assert!(rendered.contains("azimuth_error ="));
    assert!(rendered.contains("ITD_error ="));
    assert!(rendered.contains("ILD_error ="));
    assert!(rendered.contains("compliance = PASS"));
    assert!(report.passed);

    let json = report.to_json().expect("to_json failed");
    assert!(json.contains("\"passed\": true"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Pillar 8: Output-Profile Calibration (§10.5, Item 28)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pillar8_output_profile_calibration() {
    let cal_stereo = OutputCalibration::stereo(48000);
    assert_eq!(cal_stereo.layout_kind, TargetLayoutKind::Stereo);
    assert_eq!(cal_stereo.speaker_positions.len(), 2);

    let cal_714 = OutputCalibration::immersive_7_1_4(48000);
    assert_eq!(cal_714.layout_kind, TargetLayoutKind::Immersive7_1_4);
    assert_eq!(cal_714.speaker_positions.len(), 12);

    let cal_916 = OutputCalibration::immersive_9_1_6(48000);
    assert_eq!(cal_916.layout_kind, TargetLayoutKind::Immersive9_1_6);
    assert_eq!(cal_916.speaker_positions.len(), 16);

    let mut cal = OutputCalibration::stereo(48000);
    cal.gains = vec![0.5, 0.75];
    cal.polarity = vec![true, false]; // Invert left channel phase

    let mut l = vec![1.0f32; 16];
    let mut r = vec![1.0f32; 16];
    let mut planes: [&mut [f32]; 2] = [&mut l, &mut r];

    cal.apply_calibration_block(&mut planes);

    for s in &l {
        assert_eq!(*s, -0.5);
    }
    for s in &r {
        assert_eq!(*s, 0.75);
    }

    // OutputProfile integration
    let mut profile = OutputProfile::flat();
    profile.calibration = Some(cal_714);
    let json = serde_json::to_string(&profile).expect("serialize profile failed");
    let loaded: OutputProfile = serde_json::from_str(&json).expect("deserialize profile failed");
    assert!(loaded.calibration.is_some());
    assert_eq!(
        loaded.calibration.unwrap().layout_kind,
        TargetLayoutKind::Immersive7_1_4
    );
}
