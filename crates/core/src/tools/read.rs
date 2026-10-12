//! `read` — file contents, line-numbered, paged with `offset`/`limit`.
//! Image files (png/jpeg/webp/gif, magic-byte sniffed) are attached
//! for the model when the startup policy allows it, after the
//! [`super::image_resize`] ladder fits them to a universal budget
//! (AD-0009).

use super::ImageGate;
use super::truncate;
use super::{args_object, is_probably_binary, opt_str, opt_usize, resolve_path};
use crate::tool::{ImageBlock, Tool, ToolAnnotations, ToolOutput};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::path::PathBuf;

pub struct ReadTool {
    cwd: PathBuf,
    /// Whether produced images may be attached for the model — the
    /// combined `settings.images && model.vision` policy, resolved at
    /// startup and re-resolved on model switches (AD-0012): the gate
    /// is shared with the front end, which flips it when the model
    /// changes. Policy lives here, at the only place images enter the
    /// system; the wire layer stays unconditional.
    images: ImageGate,
}

impl ReadTool {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            images: ImageGate::new(false),
        }
    }

    /// Turn image attachments on (tests, and bundles that pass a bare
    /// flag). The registry path uses [`Self::images_gate`] so the
    /// front end holds the same gate.
    pub fn allow_images(mut self, yes: bool) -> Self {
        self.images = ImageGate::new(yes);
        self
    }

    /// Install the shared gate (the `registry` policy wiring).
    pub fn images_gate(mut self, gate: ImageGate) -> Self {
        self.images = gate;
        self
    }

    /// Build the image result: a metadata note for the model, plus the
    /// attachment when policy and the resize ladder allow. The note
    /// always tells the truth about what happened — attached, disabled,
    /// resized, transcoded, undecodable — so a model never wonders
    /// where its image went or why it looks different.
    fn read_image(&self, mime: &'static str, bytes: &[u8]) -> ToolOutput {
        let note = format!(
            "Read image file [{mime}, {}]",
            human_size(bytes.len() as u64)
        );
        if !self.images.allowed() {
            return ToolOutput {
                content: format!("{note} (not attached: image input is disabled for this model)"),
                images: Vec::new(),
            };
        }
        // The ladder runs once, here — before the image enters
        // history. History stores the resized bytes, so replays are
        // byte-stable (AD-0007) and the work is never redone.
        match super::image_resize::process(bytes, mime) {
            Ok(processed) => {
                let mut detail = String::new();
                if let Some((ow, oh, w, h)) = processed.resized {
                    detail.push_str(&format!(", resized {ow}×{oh} → {w}×{h}"));
                }
                if let Some(from) = processed.converted_from {
                    detail.push_str(&format!(", converted from {from}"));
                }
                ToolOutput {
                    content: format!("{note}{detail} (attached)"),
                    images: vec![ImageBlock {
                        data: STANDARD.encode(&processed.bytes),
                        mime_type: processed.mime_type.into(),
                    }],
                }
            }
            Err(err) => ToolOutput {
                content: format!("{note} (not attached: {})", err.note()),
                images: Vec::new(),
            },
        }
    }
}

/// Magic-byte sniffing, never extension guessing: a file named `.png`
/// with text inside is text, and vice versa. Covers the four formats
/// the OpenAI-compat `image_url` wire accepts.
pub(crate) fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() > 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

pub(crate) fn human_size(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb < 1024.0 {
        format!("{kb:.1} KB")
    } else {
        format!("{:.1} MB", kb / 1024.0)
    }
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a text file. Output is line-numbered and clamped to 2000 \
         lines or 50KB, whichever hits first; page through larger files with offset \
         (1-indexed) and limit. Binary files are refused. When you need more of the \
         file, call again with a later offset. Image files (png, jpeg, webp, gif) \
         are attached for you to look at when image input is enabled; otherwise \
         only their metadata comes back."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to read (relative or absolute)"
                },
                "offset": {
                    "type": "number",
                    "description": "Line number to start from (1-indexed)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of lines to return"
                }
            },
            "required": ["path"]
        })
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }

    fn prompt_note(&self) -> Option<String> {
        Some("Use read to examine files instead of cat or sed.".into())
    }
    fn execute_with_images(&self, arguments: &str) -> Result<ToolOutput, String> {
        let args = args_object(arguments)?;
        let Some(raw) = opt_str(&args, "path")? else {
            return Err("read needs a path".into());
        };
        let path = resolve_path(&self.cwd, &raw);
        let bytes =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        // Sniff before the text path — an image is "probably binary"
        // and the text path would refuse what we're about to attach.
        if let Some(mime) = sniff_image_mime(&bytes) {
            return Ok(self.read_image(mime, &bytes));
        }
        // Text (or unknown binary, which `execute` refuses with its
        // own note): one text path, unchanged behavior.
        self.execute(arguments).map(ToolOutput::from)
    }

    fn execute(&self, arguments: &str) -> Result<String, String> {
        let args = args_object(arguments)?;
        let Some(path) = opt_str(&args, "path")? else {
            return Err("read needs a path".into());
        };
        let offset = opt_usize(&args, "offset")?.unwrap_or(1);
        let limit = opt_usize(&args, "limit")?.unwrap_or(truncate::MAX_LINES);
        if offset == 0 {
            return Err("offset is 1-indexed; it must be at least 1".into());
        }
        if limit == 0 {
            return Err("limit must be at least 1".into());
        }
        let path = resolve_path(&self.cwd, &path);
        let bytes =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if is_probably_binary(&bytes) {
            return Err(format!(
                "{} looks like a binary file; read handles text only",
                path.display()
            ));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not valid UTF-8", path.display()))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok("[empty file]".into());
        }
        if offset > total {
            return Err(format!(
                "offset {offset} is past the end: the file has {total} lines"
            ));
        }
        let selected: Vec<String> = lines[offset - 1..]
            .iter()
            .take(limit)
            .enumerate()
            .map(|(i, line)| format!("{:>5}│{}", offset + i, line))
            .collect();
        let selected_count = selected.len();

        let clip = truncate::head_with(
            &selected.join("\n"),
            truncate::MAX_LINES,
            truncate::MAX_BYTES,
        );
        let mut text = clip.text;
        if clip.truncated {
            text.push_str(&format!(
                "\n[clamped at 50KB — call read again with offset {}]",
                offset + clip.kept_lines
            ));
        }
        let end = offset + selected_count - 1;
        if offset > 1 || end < total {
            text.push_str(&format!("\n[lines {offset}–{end} of {total}]"));
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hi-read-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tool(root: &Path) -> ReadTool {
        ReadTool::new(root)
    }

    #[test]
    fn reads_numbered_lines() {
        let root = temp_root("numbered");
        fs::write(root.join("f.txt"), "alpha\nbeta\n").unwrap();
        let out = tool(&root).execute(r#"{"path":"f.txt"}"#).unwrap();
        assert_eq!(out, "    1│alpha\n    2│beta");
    }

    #[test]
    fn offset_is_one_indexed_and_pages() {
        let root = temp_root("offset");
        fs::write(root.join("f.txt"), "1\n2\n3\n4\n5").unwrap();
        let out = tool(&root)
            .execute(r#"{"path":"f.txt","offset":3}"#)
            .unwrap();
        assert!(out.contains("    3│3"));
        assert!(out.contains("[lines 3–5 of 5]"));
        let out = tool(&root)
            .execute(r#"{"path":"f.txt","offset":2,"limit":2}"#)
            .unwrap();
        assert!(out.contains("[lines 2–3 of 5]"));
    }

    #[test]
    fn rejects_bad_arguments() {
        let root = temp_root("badargs");
        let t = tool(&root);
        std::fs::write(root.join("f.txt"), "1\n2\n3\n4\n5").unwrap();
        assert!(t.execute(r#"{}"#).unwrap_err().contains("path"));
        assert!(
            t.execute(r#"{"path":"f.txt","offset":0}"#)
                .unwrap_err()
                .contains("1-indexed")
        );
        assert!(
            t.execute(r#"{"path":"f.txt","limit":0}"#)
                .unwrap_err()
                .contains("at least 1")
        );
        assert!(
            t.execute(r#"{"path":"f.txt","offset":9}"#)
                .unwrap_err()
                .contains("past the end")
        );
        assert!(t.execute("not json").unwrap_err().contains("invalid JSON"));
    }

    #[test]
    fn empty_file_reports_itself() {
        let root = temp_root("empty");
        fs::write(root.join("f.txt"), "").unwrap();
        assert_eq!(
            tool(&root).execute(r#"{"path":"f.txt"}"#).unwrap(),
            "[empty file]"
        );
    }

    #[test]
    fn binary_files_are_refused() {
        let root = temp_root("binary");
        fs::write(root.join("f.bin"), [0x68, 0x69, 0x00, 0x62]).unwrap();
        let err = tool(&root).execute(r#"{"path":"f.bin"}"#).unwrap_err();
        assert!(err.contains("binary"), "got: {err}");
    }

    #[test]
    fn invalid_utf8_is_refused_cleanly() {
        let root = temp_root("utf8");
        fs::write(root.join("f.txt"), [0xff, 0xfe]).unwrap();
        let err = tool(&root).execute(r#"{"path":"f.txt"}"#).unwrap_err();
        assert!(err.contains("UTF-8"), "got: {err}");
    }

    #[test]
    fn missing_files_error_with_the_path() {
        let root = temp_root("missing");
        let err = tool(&root).execute(r#"{"path":"nope.txt"}"#).unwrap_err();
        assert!(err.contains("cannot read"), "got: {err}");
    }
    /// A real 1×1 PNG: small enough that the resize ladder must pass
    /// it through byte-identical.
    fn tiny_png_bytes() -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==")
            .unwrap()
    }

    #[test]
    fn a_shared_gate_flip_takes_effect_mid_session() {
        // AD-0012: the front end flips the gate when the model
        // changes; the registry (and this tool) never rebuild.
        let root = temp_root("gate");
        std::fs::write(root.join("shot.png"), tiny_png_bytes()).unwrap();
        let gate = ImageGate::new(true);
        let tool = ReadTool::new(&root).images_gate(gate.clone());
        let out = tool.execute_with_images(r#"{"path":"shot.png"}"#).unwrap();
        assert_eq!(out.images.len(), 1);
        gate.set(false);
        let out = tool.execute_with_images(r#"{"path":"shot.png"}"#).unwrap();
        assert!(out.images.is_empty());
        assert!(
            out.content
                .contains("not attached: image input is disabled")
        );
        gate.set(true);
        let out = tool.execute_with_images(r#"{"path":"shot.png"}"#).unwrap();
        assert_eq!(out.images.len(), 1);
    }

    #[test]
    fn images_are_attached_when_policy_allows() {
        let root = temp_root("image");
        let bytes = tiny_png_bytes();
        std::fs::write(root.join("shot.png"), &bytes).unwrap();
        let out = tool(&root)
            .allow_images(true)
            .execute_with_images(r#"{"path":"shot.png"}"#)
            .unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].mime_type, "image/png");
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&out.images[0].data)
            .unwrap();
        assert_eq!(decoded, bytes, "a fitting image is attached untouched");
        assert!(out.content.contains("Read image file [image/png"));
        assert!(out.content.contains("(attached)"));
    }

    #[test]
    fn oversized_screenshots_are_resized_with_an_honest_note() {
        let root = temp_root("bigshot");
        let img = image::DynamicImage::new_rgb8(2200, 1500);
        let mut bytes = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Png,
        )
        .unwrap();
        std::fs::write(root.join("shot.png"), &bytes).unwrap();
        let out = tool(&root)
            .allow_images(true)
            .execute_with_images(r#"{"path":"shot.png"}"#)
            .unwrap();
        assert_eq!(out.images.len(), 1);
        assert!(
            out.content
                .contains("Read image file [image/png")
                .then_some(())
                .is_some()
        );
        assert!(
            out.content
                .contains(", resized 2200×1500 → 2000×1363 (attached)")
        );
    }

    #[test]
    fn image_formats_are_sniffed_by_magic_bytes_not_extensions() {
        assert_eq!(
            sniff_image_mime(b"\xff\xd8\xff\xe0jpeg"),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_mime(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image_mime(b"RIFF1234WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_image_mime(b"just text"), None);
        // A .png name with text inside is text: content decides.
        let root = temp_root("sniff");
        std::fs::write(root.join("fake.png"), "hello\n").unwrap();
        let out = tool(&root)
            .allow_images(true)
            .execute_with_images(r#"{"path":"fake.png"}"#)
            .unwrap();
        assert!(out.images.is_empty());
        assert!(out.content.contains("1│hello"));
    }

    #[test]
    fn corrupt_image_bytes_are_noted_not_attached() {
        // Magic bytes pass the sniff; the decoder refuses what's inside.
        let root = temp_root("corrupt");
        let bytes = [0x89u8, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        std::fs::write(root.join("broken.png"), bytes).unwrap();
        let out = tool(&root)
            .allow_images(true)
            .execute_with_images(r#"{"path":"broken.png"}"#)
            .unwrap();
        assert!(out.images.is_empty());
        assert!(out.content.contains("could not be decoded"));
    }

    #[test]
    fn images_are_withheld_when_policy_disallows() {
        let root = temp_root("withheld");
        let png: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        std::fs::write(root.join("shot.png"), png).unwrap();
        // `new` is the disabled default — the startup policy wiring is
        // the only thing that turns this on.
        let out = tool(&root)
            .execute_with_images(r#"{"path":"shot.png"}"#)
            .unwrap();
        assert!(out.images.is_empty());
        assert!(out.content.contains("not attached"));
        assert!(out.content.contains("image input is disabled"));
    }
}
