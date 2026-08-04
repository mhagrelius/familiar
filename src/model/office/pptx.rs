//! Writing a presentation.
//!
//! PowerPoint's part graph is the strictest of the three: a presentation points
//! at slides, every slide points at a layout, every layout points at a master,
//! and the master carries the theme. Miss one edge and PowerPoint offers to
//! repair the file rather than open it. So the master, the layout and the theme
//! are written once as constants and every slide hangs off the same pair.
//!
//! One layout, not eleven. A generated deck is a title and some bullets, and
//! ten unused layouts are ten more things to get wrong — PowerPoint's own
//! "Blank" presentation is the same shape. Where a slide has no bullets the
//! body placeholder is left out entirely, which is what makes a section-divider
//! slide look deliberate rather than empty.
//!
//! Everything is measured in EMUs: 914,400 to the inch, 12,192,000 across a
//! 16:9 slide.

use super::markup::{spans, Span};
use super::xml::escape;
use super::zip::Archive;

/// English Metric Units per inch.
const EMU: i64 = 914_400;
/// A 16:9 slide, 13.333in × 7.5in, which is PowerPoint's default since 2013.
const WIDTH: i64 = 12_192_000;
const HEIGHT: i64 = 6_858_000;

/// One slide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Slide {
    pub title: String,
    /// Bullets, with `level` as the nesting depth from 0.
    pub bullets: Vec<Bullet>,
    /// The speaker notes, which is where the detail belongs — a slide that
    /// holds the whole argument is a document with the wrong file extension.
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bullet {
    pub level: u8,
    pub text: String,
}

impl Bullet {
    pub fn new(level: u8, text: impl Into<String>) -> Self {
        Self {
            level: level.min(4),
            text: text.into(),
        }
    }
}

impl Slide {
    pub fn titled(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Default::default()
        }
    }
}

/// Build a `.pptx`.
pub fn write(slides: &[Slide]) -> Vec<u8> {
    // A deck with no slides opens as a blank presentation rather than as a
    // repair prompt.
    let fallback = [Slide::titled("")];
    let slides = if slides.is_empty() {
        &fallback[..]
    } else {
        slides
    };

    let mut archive = Archive::new();
    archive.add_text("[Content_Types].xml", &content_types(slides));
    archive.add_text("_rels/.rels", ROOT_RELATIONSHIPS);
    archive.add_text(
        "ppt/_rels/presentation.xml.rels",
        &presentation_relationships(slides.len()),
    );
    archive.add_text("ppt/presentation.xml", &presentation(slides.len()));
    archive.add_text("ppt/slideMasters/slideMaster1.xml", MASTER);
    archive.add_text(
        "ppt/slideMasters/_rels/slideMaster1.xml.rels",
        MASTER_RELATIONSHIPS,
    );
    archive.add_text("ppt/slideLayouts/slideLayout1.xml", LAYOUT);
    archive.add_text(
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
        LAYOUT_RELATIONSHIPS,
    );
    archive.add_text("ppt/theme/theme1.xml", THEME);

    for (index, slide) in slides.iter().enumerate() {
        let number = index + 1;
        archive.add_text(&format!("ppt/slides/slide{number}.xml"), &slide_xml(slide));
        archive.add_text(
            &format!("ppt/slides/_rels/slide{number}.xml.rels"),
            &slide_relationships(number, slide.notes.is_some()),
        );
        if let Some(notes) = &slide.notes {
            archive.add_text(
                &format!("ppt/notesSlides/notesSlide{number}.xml"),
                &notes_xml(notes),
            );
            archive.add_text(
                &format!("ppt/notesSlides/_rels/notesSlide{number}.xml.rels"),
                &notes_relationships(number),
            );
        }
    }
    archive.finish()
}

fn slide_xml(slide: &Slide) -> String {
    let mut shapes = String::new();

    // The title placeholder is always present, even when empty: a slide whose
    // outline has no title is invisible in the outline pane and in every
    // export.
    shapes.push_str(&text_shape(
        2,
        "Title 1",
        r#"<p:ph type="title"/>"#,
        EMU / 2,
        EMU / 2,
        WIDTH - EMU,
        EMU * 5 / 4,
        &[paragraph(&spans(&slide.title), 0, false)],
    ));

    if !slide.bullets.is_empty() {
        let body: Vec<String> = slide
            .bullets
            .iter()
            .map(|bullet| paragraph(&spans(&bullet.text), bullet.level, true))
            .collect();
        shapes.push_str(&text_shape(
            3,
            "Content Placeholder 2",
            r#"<p:ph idx="1"/>"#,
            EMU / 2,
            EMU * 2,
            WIDTH - EMU,
            HEIGHT - EMU * 5 / 2,
            &body,
        ));
    }

    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<p:sld xmlns:a="{}" xmlns:r="{}" xmlns:p="{}">"#,
            r#"<p:cSld><p:spTree>"#,
            r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>"#,
            r#"<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/>"#,
            r#"<a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>"#,
            "{}",
            r#"</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"#
        ),
        A, R, P, shapes
    )
}

/// One text box: its placeholder role, where it sits, and its paragraphs.
#[allow(clippy::too_many_arguments)]
fn text_shape(
    id: u32,
    name: &str,
    placeholder: &str,
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    paragraphs: &[String],
) -> String {
    format!(
        concat!(
            r#"<p:sp><p:nvSpPr><p:cNvPr id="{}" name="{}"/>"#,
            r#"<p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr>{}</p:nvPr></p:nvSpPr>"#,
            r#"<p:spPr><a:xfrm><a:off x="{}" y="{}"/><a:ext cx="{}" cy="{}"/></a:xfrm></p:spPr>"#,
            r#"<p:txBody><a:bodyPr><a:normAutofit/></a:bodyPr><a:lstStyle/>{}</p:txBody></p:sp>"#
        ),
        id,
        escape(name),
        placeholder,
        x,
        y,
        width,
        height,
        paragraphs.concat()
    )
}

/// One paragraph of runs, at a bullet level.
fn paragraph(spans: &[Span], level: u8, bulleted: bool) -> String {
    let level = level.min(4);
    let properties = if bulleted {
        format!(r#"<a:pPr lvl="{level}"/>"#)
    } else {
        // `buNone` or the title inherits the master's bullet character.
        String::from(r#"<a:pPr><a:buNone/></a:pPr>"#)
    };
    let runs: String = spans
        .iter()
        .filter(|span| !span.text.is_empty())
        .map(run)
        .collect();
    // An empty paragraph still needs an end-properties element, or PowerPoint
    // loses the line entirely.
    format!("<a:p>{properties}{runs}<a:endParaRPr/></a:p>")
}

fn run(span: &Span) -> String {
    let mut properties = String::from(r#" lang="en-GB" dirty="0""#);
    if span.bold {
        properties.push_str(r#" b="1""#);
    }
    if span.italic {
        properties.push_str(r#" i="1""#);
    }
    let font = if span.code {
        r#"<a:latin typeface="Consolas"/>"#
    } else {
        ""
    };
    format!(
        r#"<a:r><a:rPr{properties}>{font}</a:rPr><a:t>{}</a:t></a:r>"#,
        escape(&span.text)
    )
}

fn notes_xml(notes: &str) -> String {
    let paragraphs: String = notes
        .lines()
        .map(|line| paragraph(&spans(line), 0, false))
        .collect();
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<p:notes xmlns:a="{}" xmlns:r="{}" xmlns:p="{}"><p:cSld><p:spTree>"#,
            r#"<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>"#,
            r#"<p:grpSpPr/>"#,
            r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Notes Placeholder 1"/>"#,
            r#"<p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>"#,
            r#"<p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr/>"#,
            r#"<p:txBody><a:bodyPr/><a:lstStyle/>{}</p:txBody></p:sp>"#,
            r#"</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:notes>"#
        ),
        A, R, P, paragraphs
    )
}

fn presentation(count: usize) -> String {
    let ids: String = (0..count)
        .map(|index| {
            // Slide ids must be at least 256 and unique; PowerPoint rejects the
            // file outright otherwise.
            format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 256 + index, index + 2)
        })
        .collect();
    format!(
        concat!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>"#,
            r#"<p:presentation xmlns:a="{}" xmlns:r="{}" xmlns:p="{}">"#,
            r#"<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>"#,
            r#"<p:sldIdLst>{}</p:sldIdLst>"#,
            r#"<p:sldSz cx="{}" cy="{}"/><p:notesSz cx="{}" cy="{}"/>"#,
            r#"</p:presentation>"#
        ),
        A, R, P, ids, WIDTH, HEIGHT, HEIGHT, WIDTH
    )
}

fn presentation_relationships(count: usize) -> String {
    let mut entries = format!(
        r#"<Relationship Id="rId1" Type="{OFFICE}/slideMaster" Target="slideMasters/slideMaster1.xml"/>"#
    );
    for index in 0..count {
        entries.push_str(&format!(
            r#"<Relationship Id="rId{}" Type="{OFFICE}/slide" Target="slides/slide{}.xml"/>"#,
            index + 2,
            index + 1
        ));
    }
    wrap_relationships(&entries)
}

fn slide_relationships(number: usize, has_notes: bool) -> String {
    let mut entries = format!(
        r#"<Relationship Id="rId1" Type="{OFFICE}/slideLayout" Target="../slideLayouts/slideLayout1.xml"/>"#
    );
    if has_notes {
        entries.push_str(&format!(
            r#"<Relationship Id="rId2" Type="{OFFICE}/notesSlide" Target="../notesSlides/notesSlide{number}.xml"/>"#
        ));
    }
    wrap_relationships(&entries)
}

fn notes_relationships(number: usize) -> String {
    wrap_relationships(&format!(
        r#"<Relationship Id="rId1" Type="{OFFICE}/slide" Target="../slides/slide{number}.xml"/>"#
    ))
}

fn wrap_relationships(entries: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="{PACKAGE}">{entries}</Relationships>"#
    )
}

fn content_types(slides: &[Slide]) -> String {
    let mut overrides = String::from(concat!(
        r#"<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>"#,
        r#"<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>"#,
        r#"<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>"#,
        r#"<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>"#,
    ));
    for (index, slide) in slides.iter().enumerate() {
        overrides.push_str(&format!(
            r#"<Override PartName="/ppt/slides/slide{}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>"#,
            index + 1
        ));
        if slide.notes.is_some() {
            overrides.push_str(&format!(
                r#"<Override PartName="/ppt/notesSlides/notesSlide{}.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/>"#,
                index + 1
            ));
        }
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/>{overrides}</Types>"#
    )
}

const A: &str = "http://schemas.openxmlformats.org/drawingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const P: &str = "http://schemas.openxmlformats.org/presentationml/2006/main";
const OFFICE: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const PACKAGE: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

const ROOT_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="ppt/presentation.xml"/></Relationships>"#;

const MASTER_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout" Target="../slideLayouts/slideLayout1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/theme" Target="../theme/theme1.xml"/></Relationships>"#;

const LAYOUT_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideMaster" Target="../slideMasters/slideMaster1.xml"/></Relationships>"#;

/// The master: the colour map, the two placeholders and the text styles every
/// slide inherits. Sizes are hundredths of a point.
const MASTER: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldMaster xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:bg><p:bgPr><a:solidFill><a:srgbClr val="FFFFFF"/></a:solidFill><a:effectLst/></p:bgPr></p:bg><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title Placeholder 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="457200" y="457200"/><a:ext cx="11277600" cy="1143000"/></a:xfrm></p:spPr><p:txBody><a:bodyPr anchor="b"/><a:lstStyle/><a:p><a:r><a:rPr lang="en-GB"/><a:t>Title</a:t></a:r></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Text Placeholder 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="body" idx="1"/></p:nvPr></p:nvSpPr><p:spPr><a:xfrm><a:off x="457200" y="1828800"/><a:ext cx="11277600" cy="4572000"/></a:xfrm></p:spPr><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:rPr lang="en-GB"/><a:t>Body</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld><p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" folHlink="folHlink"/><p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle><a:lvl1pPr algn="l"><a:defRPr sz="4000" b="1"><a:solidFill><a:srgbClr val="1F3864"/></a:solidFill><a:latin typeface="+mj-lt"/></a:defRPr></a:lvl1pPr></p:titleStyle><p:bodyStyle><a:lvl1pPr marL="285750" indent="-285750"><a:buFont typeface="Arial"/><a:buChar char="•"/><a:defRPr sz="2000"><a:solidFill><a:srgbClr val="262626"/></a:solidFill><a:latin typeface="+mn-lt"/></a:defRPr></a:lvl1pPr><a:lvl2pPr marL="742950" indent="-285750"><a:buFont typeface="Arial"/><a:buChar char="◦"/><a:defRPr sz="1800"><a:solidFill><a:srgbClr val="404040"/></a:solidFill></a:defRPr></a:lvl2pPr><a:lvl3pPr marL="1200150" indent="-285750"><a:buFont typeface="Arial"/><a:buChar char="▪"/><a:defRPr sz="1600"><a:solidFill><a:srgbClr val="404040"/></a:solidFill></a:defRPr></a:lvl3pPr><a:lvl4pPr marL="1657350" indent="-285750"><a:buFont typeface="Arial"/><a:buChar char="•"/><a:defRPr sz="1400"><a:solidFill><a:srgbClr val="595959"/></a:solidFill></a:defRPr></a:lvl4pPr><a:lvl5pPr marL="2114550" indent="-285750"><a:buFont typeface="Arial"/><a:buChar char="◦"/><a:defRPr sz="1400"><a:solidFill><a:srgbClr val="595959"/></a:solidFill></a:defRPr></a:lvl5pPr></p:bodyStyle><p:otherStyle><a:lvl1pPr><a:defRPr sz="1800"/></a:lvl1pPr></p:otherStyle></p:txStyles></p:sldMaster>"#;

/// The one layout: title and content.
const LAYOUT: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sldLayout xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main" type="obj" preserve="1"><p:cSld name="Title and Content"><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/><a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr><p:sp><p:nvSpPr><p:cNvPr id="2" name="Title 1"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="en-GB"/></a:p></p:txBody></p:sp><p:sp><p:nvSpPr><p:cNvPr id="3" name="Content Placeholder 2"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr><p:nvPr><p:ph idx="1"/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/><a:p><a:endParaRPr lang="en-GB"/></a:p></p:txBody></p:sp></p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"#;

/// The theme. PowerPoint requires one and will not open a deck without it.
const THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Familiar"><a:themeElements><a:clrScheme name="Familiar"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="1F3864"/></a:dk2><a:lt2><a:srgbClr val="F2F2F2"/></a:lt2><a:accent1><a:srgbClr val="2E5496"/></a:accent1><a:accent2><a:srgbClr val="C55A11"/></a:accent2><a:accent3><a:srgbClr val="548235"/></a:accent3><a:accent4><a:srgbClr val="7030A0"/></a:accent4><a:accent5><a:srgbClr val="BF8F00"/></a:accent5><a:accent6><a:srgbClr val="C00000"/></a:accent6><a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink></a:clrScheme><a:fontScheme name="Familiar"><a:majorFont><a:latin typeface="Calibri Light"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont><a:minorFont><a:latin typeface="Calibri"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont></a:fontScheme><a:fmtScheme name="Familiar"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn part(bytes: &[u8], name: &str) -> String {
        crate::model::office::tests::part(bytes, name)
    }

    fn deck() -> Vec<Slide> {
        vec![
            Slide {
                title: "The Design".into(),
                bullets: vec![
                    Bullet::new(0, "Local by default"),
                    Bullet::new(1, "One GPU"),
                ],
                notes: Some("Open by saying what it is for.".into()),
            },
            Slide::titled("Questions?"),
        ]
    }

    #[test]
    fn every_part_powerpoint_needs_is_present() {
        // Miss one edge of the part graph and PowerPoint offers to repair the
        // file rather than open it.
        let bytes = write(&deck());
        for required in [
            "[Content_Types].xml",
            "_rels/.rels",
            "ppt/_rels/presentation.xml.rels",
            "ppt/presentation.xml",
            "ppt/slideMasters/slideMaster1.xml",
            "ppt/slideMasters/_rels/slideMaster1.xml.rels",
            "ppt/slideLayouts/slideLayout1.xml",
            "ppt/slideLayouts/_rels/slideLayout1.xml.rels",
            "ppt/theme/theme1.xml",
            "ppt/slides/slide1.xml",
            "ppt/slides/_rels/slide1.xml.rels",
        ] {
            assert!(!part(&bytes, required).is_empty(), "{required} is missing");
        }
    }

    #[test]
    fn every_slide_points_at_the_layout() {
        let bytes = write(&deck());
        for number in 1..=2 {
            let relationships = part(&bytes, &format!("ppt/slides/_rels/slide{number}.xml.rels"));
            assert!(
                relationships.contains("slideLayouts/slideLayout1.xml"),
                "slide {number}: {relationships}"
            );
        }
    }

    #[test]
    fn slide_ids_start_at_the_minimum_powerpoint_accepts() {
        // Under 256 and PowerPoint rejects the file outright.
        let presentation = part(&write(&deck()), "ppt/presentation.xml");
        assert!(
            presentation.contains(r#"<p:sldId id="256" r:id="rId2"/>"#),
            "{presentation}"
        );
        assert!(
            presentation.contains(r#"<p:sldId id="257" r:id="rId3"/>"#),
            "{presentation}"
        );
    }

    #[test]
    fn a_title_and_its_bullets_land_in_the_right_placeholders() {
        let slide = part(&write(&deck()), "ppt/slides/slide1.xml");
        assert!(slide.contains(r#"<p:ph type="title"/>"#), "{slide}");
        assert!(slide.contains("<a:t>The Design</a:t>"), "{slide}");
        assert!(slide.contains(r#"<p:ph idx="1"/>"#), "{slide}");
        assert!(slide.contains("<a:t>Local by default</a:t>"), "{slide}");
    }

    #[test]
    fn a_nested_bullet_carries_its_level() {
        let slide = part(&write(&deck()), "ppt/slides/slide1.xml");
        assert!(slide.contains(r#"<a:pPr lvl="1"/>"#), "{slide}");
    }

    #[test]
    fn a_slide_with_no_bullets_has_no_body_placeholder() {
        // An empty content box renders as a "Click to add text" prompt in
        // edit view, which is not what a section divider should look like.
        let slide = part(&write(&deck()), "ppt/slides/slide2.xml");
        assert!(slide.contains("<a:t>Questions?</a:t>"), "{slide}");
        assert!(!slide.contains(r#"<p:ph idx="1"/>"#), "{slide}");
    }

    #[test]
    fn notes_are_written_only_for_the_slides_that_have_them() {
        let bytes = write(&deck());
        assert!(part(&bytes, "ppt/notesSlides/notesSlide1.xml")
            .contains("Open by saying what it is for."));
        assert!(part(&bytes, "ppt/notesSlides/notesSlide2.xml").is_empty());

        // And the relationship exists exactly where the part does.
        assert!(part(&bytes, "ppt/slides/_rels/slide1.xml.rels").contains("notesSlide1.xml"));
        assert!(!part(&bytes, "ppt/slides/_rels/slide2.xml.rels").contains("notesSlide"));
        assert!(part(&bytes, "[Content_Types].xml").contains("/ppt/notesSlides/notesSlide1.xml"));
    }

    #[test]
    fn a_notes_slide_points_back_at_its_slide() {
        // The edge PowerPoint checks: notes without a backlink are orphaned.
        let relationships = part(
            &write(&deck()),
            "ppt/notesSlides/_rels/notesSlide1.xml.rels",
        );
        assert!(
            relationships.contains("../slides/slide1.xml"),
            "{relationships}"
        );
    }

    #[test]
    fn the_deck_is_sixteen_by_nine() {
        let presentation = part(&write(&deck()), "ppt/presentation.xml");
        assert!(
            presentation.contains(r#"<p:sldSz cx="12192000" cy="6858000"/>"#),
            "{presentation}"
        );
    }

    #[test]
    fn bold_in_a_bullet_becomes_a_run_property() {
        let slides = vec![Slide {
            title: "T".into(),
            bullets: vec![Bullet::new(0, "plain **bold** end")],
            notes: None,
        }];
        let slide = part(&write(&slides), "ppt/slides/slide1.xml");
        assert!(slide.contains(r#"b="1""#), "{slide}");
        assert!(slide.contains("<a:t>bold</a:t>"), "{slide}");
    }

    #[test]
    fn text_that_looks_like_xml_cannot_break_a_slide() {
        let slides = vec![Slide::titled("a < b & c")];
        let slide = part(&write(&slides), "ppt/slides/slide1.xml");
        assert!(slide.contains("a &lt; b &amp; c"), "{slide}");
        assert_eq!(slide.matches("</p:sld>").count(), 1);
    }

    #[test]
    fn an_empty_deck_opens_as_a_blank_presentation() {
        let bytes = write(&[]);
        assert!(!part(&bytes, "ppt/slides/slide1.xml").is_empty());
        assert!(part(&bytes, "ppt/presentation.xml").contains(r#"id="256""#));
    }

    #[test]
    fn a_bullet_level_beyond_the_master_is_clamped_rather_than_lost() {
        // The master defines five levels; a sixth would inherit nothing and
        // render unindented with no bullet character.
        assert_eq!(Bullet::new(9, "deep").level, 4);
    }
}
