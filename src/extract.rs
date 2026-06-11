use anyhow::{Context, Result};
use std::io::Read as _;
use std::path::Path;

/// Pull the text out of a .docx (a zip containing word/document.xml). The
/// result is rough plain text; the model does the markdown formatting.
pub fn docx_to_text(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("{} is not a valid docx archive", path.display()))?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .context("docx has no word/document.xml")?
        .read_to_string(&mut xml)
        .context("reading word/document.xml")?;
    Ok(strip_docx_xml(&xml))
}

fn strip_docx_xml(xml: &str) -> String {
    let xml = xml
        .replace("</w:p>", "\n\n")
        .replace("<w:tab/>", "\t")
        .replace("<w:br/>", "\n");
    let mut out = String::with_capacity(xml.len() / 2);
    let mut in_tag = false;
    for c in xml.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    // Collapse runs of blank lines left over from paragraph markup.
    let mut result = String::with_capacity(out.len());
    let mut blanks = 0;
    for line in out.lines() {
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 1 {
                continue;
            }
        } else {
            blanks = 0;
        }
        result.push_str(line.trim_end());
        result.push('\n');
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tags_and_entities() {
        let xml = r#"<w:document><w:p><w:r><w:t>Hello &amp; goodbye</w:t></w:r></w:p><w:p><w:r><w:t>Second</w:t></w:r></w:p></w:document>"#;
        assert_eq!(strip_docx_xml(xml), "Hello & goodbye\n\nSecond");
    }
}
