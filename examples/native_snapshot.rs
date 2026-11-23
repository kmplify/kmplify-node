//! Render every section of the native app window to BMP files, for
//! documentation and review (a screen recorder needs permissions a build
//! machine does not have; the window can photograph itself).
//!
//! ```text
//! cargo run --release --example native_snapshot -- NODE_DIR OUT_DIR [light|dark] [WIDTH HEIGHT]
//! ```
//!
//! NODE_DIR is the directory of a RUNNING node (`KMPLIFY_NODE_DIR`); the
//! window attaches to it exactly as `kmplify-node window` does. One file per
//! section, `OUT_DIR/<section>-<scheme>.bmp`, then the window closes.

// Not declared as an [[example]] with required-features: the build image
// (packaging/Dockerfile.node-build) does not copy examples/, and an explicit
// target whose file is missing breaks the manifest there.
#[cfg(feature = "native-gui")]
mod snapshot {
    use std::path::{Path, PathBuf};

    use eframe::egui;
    use kmplify_node::native::{section_ids, theme, window_icon, App};

    struct Snap {
        app: App,
        ids: Vec<&'static str>,
        i: usize,
        frames: u32,
        waiting: bool,
        out: PathBuf,
        scheme: String,
        width: u32,
    }

    impl eframe::App for Snap {
        fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
            eframe::App::logic(&mut self.app, ctx, frame);
        }

        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            let shot = ui.ctx().input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            });
            if let Some(image) = shot {
                let path = self.out.join(format!(
                    "{}-{}-{}.bmp",
                    self.ids[self.i], self.scheme, self.width
                ));
                write_bmp(&path, &image).expect("write the snapshot");
                println!("{}", path.display());
                self.i += 1;
                self.frames = 0;
                self.waiting = false;
            }
            if self.i >= self.ids.len() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            self.app.set_section(self.ids[self.i]);
            eframe::App::ui(&mut self.app, ui, frame);
            self.frames += 1;
            if !self.waiting && self.frames > 20 {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                self.waiting = true;
            }
            ui.ctx().request_repaint();
        }
    }

    /// A 24-bit bottom-up BMP: no image crate for one file format.
    fn write_bmp(path: &Path, img: &egui::ColorImage) -> std::io::Result<()> {
        let (w, h) = (img.size[0] as u32, img.size[1] as u32);
        let row = (w * 3).div_ceil(4) * 4;
        let data = row * h;
        let mut out = Vec::with_capacity((54 + data) as usize);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&(54 + data).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(w as i32).to_le_bytes());
        out.extend_from_slice(&(h as i32).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&data.to_le_bytes());
        out.extend_from_slice(&[0; 16]);
        for y in (0..h).rev() {
            let start = out.len();
            for x in 0..w {
                let c = img.pixels[(y * w + x) as usize];
                out.extend_from_slice(&[c.b(), c.g(), c.r()]);
            }
            while (out.len() - start) % 4 != 0 {
                out.push(0);
            }
        }
        std::fs::write(path, out)
    }

    pub fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let (Some(dir), Some(out)) = (args.first(), args.get(1)) else {
            eprintln!("usage: native_snapshot NODE_DIR OUT_DIR [light|dark]");
            std::process::exit(2);
        };
        let scheme = args.get(2).cloned().unwrap_or_else(|| "light".into());
        let size: [f32; 2] = [
            args.get(3).and_then(|w| w.parse().ok()).unwrap_or(1180.0),
            args.get(4).and_then(|h| h.parse().ok()).unwrap_or(780.0),
        ];
        std::fs::create_dir_all(out).expect("create OUT_DIR");
        // KN_SNAPSHOT_PAIR=1: open a pairing invitation in this process
        // first, so the LAN router section shows a PIN (on loopback unless
        // the node directory has LAN mode and an allowlist).
        if std::env::var_os("KN_SNAPSHOT_PAIR").is_some() {
            let o = kmplify_node::views::perform(
                kmplify_node::views::Target::Attached,
                std::path::Path::new(dir),
                &kmplify_node::views::Act::RouterPair,
            );
            eprintln!("pairing: {}", o.message);
        }
        let snap = Snap {
            app: App::new(PathBuf::from(dir), String::new()),
            ids: section_ids(),
            i: 0,
            frames: 0,
            waiting: false,
            out: PathBuf::from(out),
            scheme: scheme.clone(),
            width: size[0] as u32,
        };
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("KMPLIFY Node")
                .with_icon(window_icon(256))
                .with_inner_size(size)
                .with_min_inner_size([300.0, 300.0]),
            ..Default::default()
        };
        eframe::run_native(
            "KMPLIFY Node snapshot",
            options,
            Box::new(move |cc| {
                theme::apply(&cc.egui_ctx);
                cc.egui_ctx.set_theme(if scheme == "dark" {
                    egui::ThemePreference::Dark
                } else {
                    egui::ThemePreference::Light
                });
                Ok(Box::new(snap))
            }),
        )
        .expect("open the window");
    }
}

#[cfg(feature = "native-gui")]
fn main() {
    snapshot::main();
}

#[cfg(not(feature = "native-gui"))]
fn main() {
    eprintln!("native_snapshot needs the native-gui feature");
}
