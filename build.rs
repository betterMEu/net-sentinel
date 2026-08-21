use std::{env, fs::File, io::BufWriter, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is required"));
    let icon_path = output.join("net-sentinel.ico");
    let mut directory = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16, 24, 32, 48, 64, 128, 256] {
        let image = ico::IconImage::from_rgba_data(size, size, icon_pixels(size));
        directory.add_entry(ico::IconDirEntry::encode(&image).expect("encode application icon"));
    }
    directory
        .write(BufWriter::new(
            File::create(&icon_path).expect("create application icon"),
        ))
        .expect("write application icon");

    let resource_path = output.join("net-sentinel.rc");
    let icon_reference = icon_path.to_string_lossy().replace('\\', "/");
    std::fs::write(&resource_path, format!("1 ICON \"{icon_reference}\"\n"))
        .expect("write application resources");
    embed_resource::compile(&resource_path, embed_resource::NONE)
        .manifest_optional()
        .expect("compile Windows application resources");
}

fn icon_pixels(size: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    let edge = 2.0 / size as f32;
    for y in 0..size {
        for x in 0..size {
            let nx = (x as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let ny = (y as f32 + 0.5) / size as f32 * 2.0 - 1.0;
            let distance = (nx * nx + ny * ny).sqrt();
            if distance > 0.94 {
                pixels.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            }

            let mut color = [48u8, 91u8, 129u8, 255u8];
            let wave = 0.12 + (nx * std::f32::consts::PI * 1.6).sin() * 0.10;
            if ny > wave {
                color = [49, 157, 205, 255];
            }
            if distance > 0.94 - edge * 2.2 {
                color = [224, 244, 252, 255];
            }
            let slash = (ny + nx * 0.72).abs() < 0.075 + edge * 0.5;
            if slash && nx > -0.58 && nx < 0.58 && ny > -0.62 && ny < 0.62 {
                color = [255, 255, 255, 255];
            }
            pixels.extend_from_slice(&color);
        }
    }
    pixels
}
