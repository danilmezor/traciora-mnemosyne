//! Standard Markdown structure via `pulldown-cmark`: headings and
//! `[text](url)` links. Wikilinks and tags are handled by [`crate::link`].

use pulldown_cmark::{Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};

/// A Markdown ATX/setext heading.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Heading {
    /// 1-6, matching Markdown heading levels.
    pub level: u8,
    /// The heading text with inline markup flattened to plain text.
    pub text: String,
    /// Byte offset of the heading marker.
    pub start: usize,
    /// Byte offset one past the heading line.
    pub end: usize,
}

/// A standard Markdown link.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MarkdownLink {
    /// The link destination (URL or path).
    pub destination: String,
    /// The link text.
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// The result of parsing Markdown structure.
#[derive(Debug, Default, Clone)]
pub struct Markdown {
    pub headings: Vec<Heading>,
    pub links: Vec<MarkdownLink>,
    pub images: Vec<MarkdownLink>,
}

/// Extracts headings, links, and images with byte offsets.
pub fn parse(source: &str) -> Markdown {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);

    let mut headings: Vec<Heading> = Vec::new();
    let mut links: Vec<MarkdownLink> = Vec::new();
    let mut images: Vec<MarkdownLink> = Vec::new();
    let mut link_stack: Vec<(String, String, usize, bool, bool)> = Vec::new();
    let mut heading_stack: Vec<(HeadingLevel, String, usize)> = Vec::new();

    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        match event {
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) => {
                let is_inline = link_type == LinkType::Inline;
                let dest = if link_type == LinkType::Email {
                    format!("mailto:{}", dest_url.into_string())
                } else {
                    dest_url.into_string()
                };
                link_stack.push((dest, String::new(), range.start, false, is_inline));
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                ..
            }) => {
                let is_inline = link_type == LinkType::Inline;
                link_stack.push((
                    dest_url.into_string(),
                    String::new(),
                    range.start,
                    true,
                    is_inline,
                ));
            }
            Event::End(TagEnd::Link) | Event::End(TagEnd::Image) => {
                if let Some((destination, text, start, is_image, is_inline)) = link_stack.pop() {
                    if is_inline {
                        let entry = MarkdownLink {
                            destination,
                            text,
                            start,
                            end: range.end,
                        };
                        if is_image {
                            images.push(entry);
                        } else {
                            links.push(entry);
                        }
                    }
                }
            }
            Event::Start(Tag::Heading { level, .. }) => {
                heading_stack.push((level, String::new(), range.start));
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, text, start)) = heading_stack.pop() {
                    headings.push(Heading {
                        level: heading_level_u8(level),
                        text,
                        start,
                        end: range.end,
                    });
                }
            }
            Event::Text(t) | Event::Code(t) => {
                let s = t.into_string();
                if let Some(l) = link_stack.last_mut() {
                    l.1.push_str(&s);
                }
                if let Some(h) = heading_stack.last_mut() {
                    h.1.push_str(&s);
                }
            }
            _ => {}
        }
    }

    Markdown {
        headings,
        links,
        images,
    }
}

fn heading_level_u8(level: HeadingLevel) -> u8 {
    if level == HeadingLevel::H1 {
        1
    } else if level == HeadingLevel::H2 {
        2
    } else if level == HeadingLevel::H3 {
        3
    } else if level == HeadingLevel::H4 {
        4
    } else if level == HeadingLevel::H5 {
        5
    } else {
        6
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headings() {
        let hs = parse("# One\n\n## Two\n\ntext").headings;
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].level, 1);
        assert_eq!(hs[0].text, "One");
        assert_eq!(hs[1].level, 2);
        assert_eq!(hs[1].text, "Two");
    }

    #[test]
    fn markdown_links() {
        let links = parse("[docs](https://example.com)").links;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].destination, "https://example.com");
        assert_eq!(links[0].text, "docs");
    }

    #[test]
    fn links_in_code_are_skipped() {
        let links = parse("`[x](y)`").links;
        assert!(links.is_empty());
    }

    #[test]
    fn images() {
        let images = parse("![alt text](./img.png)").images;
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].destination, "./img.png");
        assert_eq!(images[0].text, "alt text");
    }
}
