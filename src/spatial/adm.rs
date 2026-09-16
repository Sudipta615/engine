//! ADM (Audio Definition Model, ITU-R BS.2076) scene mapping and XML codec (§4.1, Item 23).
//!
//! Enables round-trip translation between the engine's internal [`SpatialScene`]
//! and standardized ADM metadata documents.

use thiserror::Error;

use super::math::Vec3;
use super::SpatialScene;
use crate::decode::ChannelLayout;
use crate::standards::adm::*;

/// Errors that may occur during ADM document conversion and XML parsing.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AdmError {
    #[error("XML parsing error: {0}")]
    XmlParse(String),
    #[error("missing required ADM field: {0}")]
    MissingField(String),
    #[error("unsupported ADM feature: {0}")]
    Unsupported(String),
    #[error("invalid coordinate or dimension in ADM: {0}")]
    InvalidCoordinate(String),
}

/// Bidirectional converter between [`AdmDocument`] and [`SpatialScene`].
pub struct AdmSceneConverter;

impl AdmSceneConverter {
    /// Convert an [`AdmDocument`] into an internal [`SpatialScene`].
    pub fn to_spatial_scene(doc: &AdmDocument) -> Result<SpatialScene, AdmError> {
        let mut scene = SpatialScene::new(48000);

        // 1. Convert AudioObjects
        for obj in &doc.objects {
            // Find corresponding channel format blocks for coordinates
            let mut pos = Vec3::new(0.0, 1.0, 0.0);
            let mut gain = 1.0f32;
            let mut spread = 0.0f32;
            let mut divergence = 0.0f32;
            let mut diffuseness = 0.0f32;

            // Search associated pack formats and channel formats
            for pack_id in &obj.pack_formats {
                if let Some(pack) = doc.pack_formats.iter().find(|p| &p.id == pack_id) {
                    for ch_id in &pack.channel_formats {
                        if let Some(ch) = doc.channel_formats.iter().find(|c| &c.id == ch_id) {
                            if let Some(block) = ch.blocks.first() {
                                // Extract coordinates: polar preferred if present, else cartesian
                                if let (Some(az), Some(el)) = (block.azimuth, block.elevation) {
                                    let dist = block.distance.unwrap_or(1.0);
                                    let (x, y, z) = adm_polar_to_cartesian(az, el, dist);
                                    pos = Vec3::new(x, y, z);
                                } else if let (Some(x), Some(y), Some(z)) =
                                    (block.x, block.y, block.z)
                                {
                                    pos = Vec3::new(x, y, z);
                                }

                                if let Some(g) = block.gain {
                                    gain = g;
                                }
                                if let Some(s) = block.spread {
                                    spread = s;
                                }
                                if let Some(d) = block.divergence {
                                    divergence = d;
                                }
                                if let Some(diff) = block.diffuseness {
                                    diffuseness = diff;
                                }
                            }
                        }
                    }
                }
            }

            let obj_id = scene.create_audio_object(pos).ok_or_else(|| {
                AdmError::Unsupported("scene object capacity exceeded".to_string())
            })?;

            if let Some(scene_obj) = scene.object_mut(obj_id) {
                scene_obj.gain = gain;
                scene_obj.spread = spread;
                scene_obj.divergence = divergence;
                scene_obj.diffuseness = diffuseness;
                scene_obj.head_locked = obj.head_locked;
                scene_obj.screen_relative = obj.screen_relative;
            }
        }

        // 2. Convert DirectSpeakers packs into Beds
        for pack in &doc.pack_formats {
            if pack.audio_type == AudioTypeDefinition::DirectSpeakers {
                let channels = pack.channel_formats.len();
                let layout = ChannelLayout::from_count(channels);
                let _ = scene.create_bed(layout);
            } else if pack.audio_type == AudioTypeDefinition::Hoa {
                let _ = scene.create_field();
            }
        }

        Ok(scene)
    }

    /// Convert a [`SpatialScene`] into an [`AdmDocument`].
    pub fn from_spatial_scene(scene: &SpatialScene, standard: AdmStandard) -> AdmDocument {
        let mut doc = AdmDocument {
            standard,
            programmes: Vec::new(),
            contents: Vec::new(),
            objects: Vec::new(),
            pack_formats: Vec::new(),
            channel_formats: Vec::new(),
            stream_formats: Vec::new(),
            track_formats: Vec::new(),
            zones: Vec::new(),
        };

        // Master Programme & Content
        let prog_id = "APR_1001".to_string();
        let content_id = "ACO_1001".to_string();
        let mut content_object_ids = Vec::new();

        for (idx, obj) in scene.objects.iter().enumerate() {
            let obj_id_str = format!("AO_{:04}", idx + 1001);
            let pack_id_str = format!("AP_{:04}", idx + 1001);
            let ch_id_str = format!("AC_{:04}", idx + 1001);

            let (az_deg, el_deg, dist_m) =
                cartesian_to_adm_polar(obj.position.x, obj.position.y, obj.position.z);

            let block = AudioBlockFormat {
                id: format!("AB_{:04}_0001", idx + 1001),
                r#type: AudioTypeDefinition::Objects,
                start: 0.0,
                duration: 0.0,
                x: Some(obj.position.x),
                y: Some(obj.position.y),
                z: Some(obj.position.z),
                azimuth: Some(az_deg),
                elevation: Some(el_deg),
                distance: Some(dist_m),
                gain: Some(obj.gain),
                spread: Some(obj.spread),
                divergence: Some(obj.divergence),
                width: Some(obj.extent.width),
                height: Some(obj.extent.height),
                depth: Some(obj.extent.depth),
                diffuseness: Some(obj.diffuseness),
                screen_ref: obj.screen_relative,
                order: None,
                degree: None,
                normalization: None,
            };

            let channel_format = AudioChannelFormat {
                id: ch_id_str.clone(),
                name: format!("Object Channel {:04}", idx + 1),
                audio_type: AudioTypeDefinition::Objects,
                blocks: vec![block],
            };

            let pack_format = AudioPackFormat {
                id: pack_id_str.clone(),
                name: format!("Object Pack {:04}", idx + 1),
                audio_type: AudioTypeDefinition::Objects,
                channel_formats: vec![ch_id_str],
            };

            let audio_obj = AudioObject {
                id: obj_id_str.clone(),
                name: format!("Spatial Object {:04}", idx + 1),
                start: Some(0.0),
                duration: None,
                head_locked: obj.head_locked,
                screen_relative: obj.screen_relative,
                pack_formats: vec![pack_id_str],
                track_formats: Vec::new(),
            };

            content_object_ids.push(obj_id_str);
            doc.objects.push(audio_obj);
            doc.pack_formats.push(pack_format);
            doc.channel_formats.push(channel_format);
        }

        // Add beds
        for (idx, bed) in scene.beds.iter().enumerate() {
            let pack_id_str = format!("AP_BED_{:04}", idx + 1001);
            let pack_format = AudioPackFormat {
                id: pack_id_str.clone(),
                name: format!("Bed_{:04}", bed.id.0),
                audio_type: AudioTypeDefinition::DirectSpeakers,
                channel_formats: Vec::new(),
            };
            doc.pack_formats.push(pack_format);
        }

        // Add fields
        for (idx, field) in scene.fields.iter().enumerate() {
            let pack_id_str = format!("AP_HOA_{:04}", idx + 1001);
            let pack_format = AudioPackFormat {
                id: pack_id_str.clone(),
                name: format!("Field_{:04}", field.id.0),
                audio_type: AudioTypeDefinition::Hoa,
                channel_formats: Vec::new(),
            };
            doc.pack_formats.push(pack_format);
        }

        doc.contents.push(AudioContent {
            id: content_id.clone(),
            name: "Main Content".to_string(),
            dialogue: None,
            objects: content_object_ids,
        });

        doc.programmes.push(AudioProgramme {
            id: prog_id,
            name: "Master Programme".to_string(),
            language: Some("und".to_string()),
            max_loudness: None,
            contents: vec![content_id],
        });

        doc
    }
}

/// Serialize an [`AdmDocument`] into ITU-R BS.2076 ADM XML.
pub fn to_adm_xml(doc: &AdmDocument) -> String {
    let mut xml = String::with_capacity(4096);
    xml.push_str("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n");
    xml.push_str("<ituBS2076 version=\"BS.2076-2\">\n");
    xml.push_str("  <coreMetadata>\n");

    // Programmes
    for p in &doc.programmes {
        xml.push_str(&format!(
            "    <audioProgramme audioProgrammeID=\"{}\" audioProgrammeName=\"{}\">\n",
            p.id, p.name
        ));
        for c in &p.contents {
            xml.push_str(&format!(
                "      <audioContentIDRef>{}</audioContentIDRef>\n",
                c
            ));
        }
        xml.push_str("    </audioProgramme>\n");
    }

    // Contents
    for c in &doc.contents {
        xml.push_str(&format!(
            "    <audioContent audioContentID=\"{}\" audioContentName=\"{}\">\n",
            c.id, c.name
        ));
        for o in &c.objects {
            xml.push_str(&format!(
                "      <audioObjectIDRef>{}</audioObjectIDRef>\n",
                o
            ));
        }
        xml.push_str("    </audioContent>\n");
    }

    // Objects
    for o in &doc.objects {
        xml.push_str(&format!(
            "    <audioObject audioObjectID=\"{}\" audioObjectName=\"{}\" headLocked=\"{}\" screenRelative=\"{}\">\n",
            o.id, o.name, o.head_locked, o.screen_relative
        ));
        for p in &o.pack_formats {
            xml.push_str(&format!(
                "      <audioPackFormatIDRef>{}</audioPackFormatIDRef>\n",
                p
            ));
        }
        xml.push_str("    </audioObject>\n");
    }

    // PackFormats
    for p in &doc.pack_formats {
        let type_str = match p.audio_type {
            AudioTypeDefinition::DirectSpeakers => "DirectSpeakers",
            AudioTypeDefinition::Matrix => "Matrix",
            AudioTypeDefinition::Objects => "Objects",
            AudioTypeDefinition::Hoa => "HOA",
            AudioTypeDefinition::Binaural => "Binaural",
        };
        xml.push_str(&format!(
            "    <audioPackFormat audioPackFormatID=\"{}\" audioPackFormatName=\"{}\" typeDefinition=\"{}\">\n",
            p.id, p.name, type_str
        ));
        for c in &p.channel_formats {
            xml.push_str(&format!(
                "      <audioChannelFormatIDRef>{}</audioChannelFormatIDRef>\n",
                c
            ));
        }
        xml.push_str("    </audioPackFormat>\n");
    }

    // ChannelFormats & BlockFormats
    for c in &doc.channel_formats {
        let type_str = match c.audio_type {
            AudioTypeDefinition::DirectSpeakers => "DirectSpeakers",
            AudioTypeDefinition::Matrix => "Matrix",
            AudioTypeDefinition::Objects => "Objects",
            AudioTypeDefinition::Hoa => "HOA",
            AudioTypeDefinition::Binaural => "Binaural",
        };
        xml.push_str(&format!(
            "    <audioChannelFormat audioChannelFormatID=\"{}\" audioChannelFormatName=\"{}\" typeDefinition=\"{}\">\n",
            c.id, c.name, type_str
        ));
        for b in &c.blocks {
            xml.push_str(&format!(
                "      <audioBlockFormat audioBlockFormatID=\"{}\" rType=\"{}\">\n",
                b.id, type_str
            ));
            if let (Some(x), Some(y), Some(z)) = (b.x, b.y, b.z) {
                xml.push_str(&format!(
                    "        <position coordinate=\"X\">{:.6}</position>\n",
                    x
                ));
                xml.push_str(&format!(
                    "        <position coordinate=\"Y\">{:.6}</position>\n",
                    y
                ));
                xml.push_str(&format!(
                    "        <position coordinate=\"Z\">{:.6}</position>\n",
                    z
                ));
            }
            if let (Some(az), Some(el)) = (b.azimuth, b.elevation) {
                xml.push_str(&format!(
                    "        <position coordinate=\"azimuth\">{:.4}</position>\n",
                    az
                ));
                xml.push_str(&format!(
                    "        <position coordinate=\"elevation\">{:.4}</position>\n",
                    el
                ));
                if let Some(d) = b.distance {
                    xml.push_str(&format!(
                        "        <position coordinate=\"distance\">{:.4}</position>\n",
                        d
                    ));
                }
            }
            if let Some(g) = b.gain {
                xml.push_str(&format!("        <gain>{:.4}</gain>\n", g));
            }
            if let Some(s) = b.spread {
                xml.push_str(&format!("        <spread>{:.4}</spread>\n", s));
            }
            if let Some(d) = b.divergence {
                xml.push_str(&format!("        <divergence>{:.4}</divergence>\n", d));
            }
            if let Some(diff) = b.diffuseness {
                xml.push_str(&format!("        <diffuseness>{:.4}</diffuseness>\n", diff));
            }
            xml.push_str("      </audioBlockFormat>\n");
        }
        xml.push_str("    </audioChannelFormat>\n");
    }

    xml.push_str("  </coreMetadata>\n");
    xml.push_str("</ituBS2076>\n");
    xml
}

/// Parse ITU-R BS.2076 ADM XML metadata string into an [`AdmDocument`].
pub fn parse_adm_xml(xml: &str) -> Result<AdmDocument, AdmError> {
    let mut doc = AdmDocument {
        standard: AdmStandard::ItuBs2076_2,
        programmes: Vec::new(),
        contents: Vec::new(),
        objects: Vec::new(),
        pack_formats: Vec::new(),
        channel_formats: Vec::new(),
        stream_formats: Vec::new(),
        track_formats: Vec::new(),
        zones: Vec::new(),
    };

    // Lightweight token-based XML scanner for ITU-R BS.2076 coreMetadata tags
    let lines: Vec<&str> = xml.lines().map(|l| l.trim()).collect();
    let mut current_obj: Option<AudioObject> = None;
    let mut current_pack: Option<AudioPackFormat> = None;
    let mut current_ch: Option<AudioChannelFormat> = None;
    let mut current_block: Option<AudioBlockFormat> = None;

    for line in lines {
        if line.starts_with("<audioObject ") {
            let id =
                extract_attr(line, "audioObjectID").unwrap_or_else(|| "AO_unknown".to_string());
            let name = extract_attr(line, "audioObjectName").unwrap_or_default();
            let head_locked = extract_attr(line, "headLocked")
                .map(|v| v == "true")
                .unwrap_or(false);
            let screen_relative = extract_attr(line, "screenRelative")
                .map(|v| v == "true")
                .unwrap_or(false);
            current_obj = Some(AudioObject {
                id,
                name,
                start: Some(0.0),
                duration: None,
                head_locked,
                screen_relative,
                pack_formats: Vec::new(),
                track_formats: Vec::new(),
            });
        } else if line.starts_with("<audioPackFormatIDRef>") {
            if let Some(ref mut obj) = current_obj {
                if let Some(val) = extract_tag_value(line, "audioPackFormatIDRef") {
                    obj.pack_formats.push(val);
                }
            }
        } else if line == "</audioObject>" {
            if let Some(obj) = current_obj.take() {
                doc.objects.push(obj);
            }
        } else if line.starts_with("<audioPackFormat ") {
            let id =
                extract_attr(line, "audioPackFormatID").unwrap_or_else(|| "AP_unknown".to_string());
            let name = extract_attr(line, "audioPackFormatName").unwrap_or_default();
            let type_str = extract_attr(line, "typeDefinition").unwrap_or_default();
            let audio_type = match type_str.as_str() {
                "DirectSpeakers" => AudioTypeDefinition::DirectSpeakers,
                "HOA" => AudioTypeDefinition::Hoa,
                "Binaural" => AudioTypeDefinition::Binaural,
                "Matrix" => AudioTypeDefinition::Matrix,
                _ => AudioTypeDefinition::Objects,
            };
            current_pack = Some(AudioPackFormat {
                id,
                name,
                audio_type,
                channel_formats: Vec::new(),
            });
        } else if line.starts_with("<audioChannelFormatIDRef>") {
            if let Some(ref mut pack) = current_pack {
                if let Some(val) = extract_tag_value(line, "audioChannelFormatIDRef") {
                    pack.channel_formats.push(val);
                }
            }
        } else if line == "</audioPackFormat>" {
            if let Some(pack) = current_pack.take() {
                doc.pack_formats.push(pack);
            }
        } else if line.starts_with("<audioChannelFormat ") {
            let id = extract_attr(line, "audioChannelFormatID")
                .unwrap_or_else(|| "AC_unknown".to_string());
            let name = extract_attr(line, "audioChannelFormatName").unwrap_or_default();
            let type_str = extract_attr(line, "typeDefinition").unwrap_or_default();
            let audio_type = match type_str.as_str() {
                "DirectSpeakers" => AudioTypeDefinition::DirectSpeakers,
                "HOA" => AudioTypeDefinition::Hoa,
                "Binaural" => AudioTypeDefinition::Binaural,
                "Matrix" => AudioTypeDefinition::Matrix,
                _ => AudioTypeDefinition::Objects,
            };
            current_ch = Some(AudioChannelFormat {
                id,
                name,
                audio_type,
                blocks: Vec::new(),
            });
        } else if line.starts_with("<audioBlockFormat ") {
            let id = extract_attr(line, "audioBlockFormatID")
                .unwrap_or_else(|| "AB_unknown".to_string());
            current_block = Some(AudioBlockFormat {
                id,
                r#type: AudioTypeDefinition::Objects,
                start: 0.0,
                duration: 0.0,
                x: None,
                y: None,
                z: None,
                azimuth: None,
                elevation: None,
                distance: None,
                gain: None,
                spread: None,
                divergence: None,
                width: None,
                height: None,
                depth: None,
                diffuseness: None,
                screen_ref: false,
                order: None,
                degree: None,
                normalization: None,
            });
        } else if line.contains("<position coordinate=\"X\">") {
            if let Some(ref mut blk) = current_block {
                blk.x = extract_tag_value(line, "position").and_then(|v| v.parse().ok());
            }
        } else if line.contains("<position coordinate=\"Y\">") {
            if let Some(ref mut blk) = current_block {
                blk.y = extract_tag_value(line, "position").and_then(|v| v.parse().ok());
            }
        } else if line.contains("<position coordinate=\"Z\">") {
            if let Some(ref mut blk) = current_block {
                blk.z = extract_tag_value(line, "position").and_then(|v| v.parse().ok());
            }
        } else if line.contains("<position coordinate=\"azimuth\">") {
            if let Some(ref mut blk) = current_block {
                blk.azimuth = extract_tag_value(line, "position").and_then(|v| v.parse().ok());
            }
        } else if line.contains("<position coordinate=\"elevation\">") {
            if let Some(ref mut blk) = current_block {
                blk.elevation = extract_tag_value(line, "position").and_then(|v| v.parse().ok());
            }
        } else if line.contains("<position coordinate=\"distance\">") {
            if let Some(ref mut blk) = current_block {
                blk.distance = extract_tag_value(line, "position").and_then(|v| v.parse().ok());
            }
        } else if line.starts_with("<gain>") {
            if let Some(ref mut blk) = current_block {
                blk.gain = extract_tag_value(line, "gain").and_then(|v| v.parse().ok());
            }
        } else if line.starts_with("<spread>") {
            if let Some(ref mut blk) = current_block {
                blk.spread = extract_tag_value(line, "spread").and_then(|v| v.parse().ok());
            }
        } else if line.starts_with("<divergence>") {
            if let Some(ref mut blk) = current_block {
                blk.divergence = extract_tag_value(line, "divergence").and_then(|v| v.parse().ok());
            }
        } else if line.starts_with("<diffuseness>") {
            if let Some(ref mut blk) = current_block {
                blk.diffuseness =
                    extract_tag_value(line, "diffuseness").and_then(|v| v.parse().ok());
            }
        } else if line == "</audioBlockFormat>" {
            if let (Some(ref mut ch), Some(blk)) = (current_ch.as_mut(), current_block.take()) {
                ch.blocks.push(blk);
            }
        } else if line == "</audioChannelFormat>" {
            if let Some(ch) = current_ch.take() {
                doc.channel_formats.push(ch);
            }
        }
    }

    Ok(doc)
}

fn extract_attr(line: &str, attr_name: &str) -> Option<String> {
    let needle = format!("{}=\"", attr_name);
    let start = line.find(&needle)? + needle.len();
    let end = line[start..].find('\"')? + start;
    Some(line[start..end].to_string())
}

fn extract_tag_value(line: &str, tag_name: &str) -> Option<String> {
    let open_needle = format!("<{}", tag_name);
    let open_idx = line.find(&open_needle)?;
    let content_start = line[open_idx..].find('>')? + open_idx + 1;
    let close_needle = format!("</{}>", tag_name);
    let close_idx = line.find(&close_needle)?;
    Some(line[content_start..close_idx].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_adm_xml_round_trip() {
        let mut scene = SpatialScene::new(48000);
        let obj_id = scene.create_audio_object(Vec3::new(1.0, 2.0, 0.5)).unwrap();
        if let Some(obj) = scene.object_mut(obj_id) {
            obj.gain = 0.8;
            obj.spread = 0.3;
            obj.divergence = 0.5;
            obj.diffuseness = 0.2;
            obj.head_locked = true;
        }

        let doc = AdmSceneConverter::from_spatial_scene(&scene, AdmStandard::ItuBs2076_2);
        assert_eq!(doc.objects.len(), 1);
        assert!(doc.objects[0].head_locked);

        let xml = to_adm_xml(&doc);
        assert!(xml.contains("headLocked=\"true\""));
        assert!(xml.contains("<gain>0.8000</gain>"));

        let parsed_doc = parse_adm_xml(&xml).unwrap();
        assert_eq!(parsed_doc.objects.len(), 1);
        assert_eq!(parsed_doc.pack_formats.len(), 1);
        assert_eq!(parsed_doc.channel_formats.len(), 1);
        assert_eq!(parsed_doc.channel_formats[0].blocks.len(), 1);

        let round_scene = AdmSceneConverter::to_spatial_scene(&parsed_doc).unwrap();
        assert_eq!(round_scene.objects.len(), 1);
        let round_obj = round_scene.objects.iter().next().unwrap();
        assert!(round_obj.head_locked);
        assert!((round_obj.gain - 0.8).abs() < 1e-3);
        assert!((round_obj.spread - 0.3).abs() < 1e-3);
        assert!((round_obj.divergence - 0.5).abs() < 1e-3);
        assert!((round_obj.diffuseness - 0.2).abs() < 1e-3);
    }
}
