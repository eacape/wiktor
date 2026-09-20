//! 种子 Wiki 页面解析。
//!
//! Step 2 用手工 Markdown 页面模拟编译产物（不接 LLM）：frontmatter 交由
//! `gray_matter` 解析（YAML → JSON Value → 反序列化为 [`SeedFrontmatter`]），
//! 正文用 `pulldown-cmark` 的 offset 事件流按 `##` 二级标题切章，导语归入
//! 首节（heading=`概述`）。不手写 frontmatter 边界 / 标题行匹配这类易错解析。
//!
//! 页面契约见 `docs/design/step2-seed-wiki-query-loop.md` §3。

use crate::types::error::{Error, Result};
use crate::types::{EntityId, PageMetadata, Section, WikiPage};

/// seed-wiki 页面 frontmatter（YAML，解析自 `gray_matter` 的 JSON Value）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SeedFrontmatter {
    pub page_id: String,
    pub entity_id: String,
    pub title: String,
    pub entity_type: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// 解析一个 seed-wiki Markdown 文件内容 → `WikiPage`。
///
/// 文件格式：`---` YAML frontmatter + Markdown 正文（`## ` 二级标题分节）。
pub fn parse_page(content: &str) -> Result<WikiPage> {
    // frontmatter 用 gray_matter 解析（YAML engine，不手写分隔符边界逻辑）
    let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
    let result = matter.parse(content);
    let data = result.data.ok_or_else(|| {
        Error::Validation("seed page must have YAML frontmatter (--- ... ---)".into())
    })?;
    let frontmatter: SeedFrontmatter = data
        .deserialize()
        .map_err(|e| Error::Validation(format!("invalid frontmatter: {e}")))?;

    // 校验 page_id / entity_id 为合法实体 key（domain:type:id）
    let page_id = frontmatter.page_id.trim();
    if page_id.is_empty() {
        return Err(Error::Validation(
            "seed frontmatter: page_id is empty".into(),
        ));
    }
    EntityId::from_key(page_id)?;
    let entity_id = EntityId::from_key(frontmatter.entity_id.trim())?;

    let body = result.content;
    let sections = split_sections(&body);

    Ok(WikiPage {
        page_id: page_id.to_string(),
        entity_id,
        title: frontmatter.title.trim().to_string(),
        content: body.to_string(),
        sections,
        metadata: PageMetadata {
            // domain_pack_version 由调用方（CLI 从 domain.yaml）填充
            domain_pack_version: String::new(),
            compiled_at: unix_now(),
            model_version: "seed-manual".into(),
            embedding_model: "none".into(),
        },
    })
}

/// 用 pulldown-cmark 的 offset 事件流按 H2 切分正文（§3.3/§3.4）。
///
/// - `##` 二级标题开启新节，节 content 为「本标题行起，到下一标题行前」的原文；
/// - `##` 之前的导语内容（非空）归为首节，heading = `概述`；
/// - `###` 及更低级标题留原文，不细分。
fn split_sections(body: &str) -> Vec<Section> {
    use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

    // 收集 H2 标题的 (起始偏移, 结束偏移, 标题文本)
    let mut headings: Vec<(usize, usize, String)> = Vec::new();
    let mut in_h2 = false;
    let mut h2_text = String::new();
    let mut h2_start = 0usize;
    for (event, range) in Parser::new(body).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H2,
                ..
            }) => {
                in_h2 = true;
                h2_text.clear();
                h2_start = range.start;
            }
            Event::End(TagEnd::Heading(HeadingLevel::H2)) => {
                in_h2 = false;
                headings.push((h2_start, range.end, h2_text.trim().to_string()));
            }
            Event::Text(t) if in_h2 => h2_text.push_str(&t),
            _ => {}
        }
    }

    let mut sections = Vec::new();
    if headings.is_empty() {
        // 无任何 H2：整段作导语
        let intro = body.trim();
        if !intro.is_empty() {
            sections.push(Section {
                heading: "概述".into(),
                content: intro.to_string(),
            });
        }
        return sections;
    }

    // 导语（首个 H2 之前的内容）归为首节
    let intro = body[..headings[0].0].trim();
    if !intro.is_empty() {
        sections.push(Section {
            heading: "概述".into(),
            content: intro.to_string(),
        });
    }

    for i in 0..headings.len() {
        let (start, _, heading) = &headings[i];
        let end = if i + 1 < headings.len() {
            headings[i + 1].0
        } else {
            body.len()
        };
        sections.push(Section {
            heading: heading.clone(),
            content: body[*start..end].trim().to_string(),
        });
    }
    sections
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
---
page_id: milk-tea:drink:boba-milk-tea
entity_id: milk-tea:drink:boba-milk-tea
title: 波霸奶茶
entity_type: drink
aliases: [珍珠奶茶, 波霸]
tags: [奶茶, 经典]
---

波霸奶茶是以红茶为基底、加入波霸珍珠的经典台湾奶茶。

## 成分

- 红茶
- 波霸珍珠
- 鲜奶或奶精

## 口感与特征

珍珠软糯，茶味浓郁。
";

    #[test]
    fn parses_frontmatter_and_sections() {
        let page = parse_page(SAMPLE).unwrap();
        assert_eq!(page.page_id, "milk-tea:drink:boba-milk-tea");
        assert_eq!(page.entity_id.id, "boba-milk-tea");
        assert_eq!(page.title, "波霸奶茶");
        assert_eq!(page.metadata.model_version, "seed-manual");

        // 导语归首节（概述）+ 成分 + 口感与特征
        assert_eq!(page.sections.len(), 3);
        assert_eq!(page.sections[0].heading, "概述");
        assert!(page.sections[0].content.contains("波霸珍珠"));
        assert_eq!(page.sections[1].heading, "成分");
        assert!(page.sections[1].content.contains("- 红茶"));
        assert_eq!(page.sections[2].heading, "口感与特征");
        assert!(page.sections[2].content.contains("珍珠软糯"));
    }

    #[test]
    fn rejects_bad_frontmatter() {
        // 不含 frontmatter
        assert!(parse_page("no frontmatter here").is_err());
        // 缺闭合（gray_matter 视为无 frontmatter → data 为空）
        assert!(parse_page("page_id: x\n").is_err());
    }

    #[test]
    fn rejects_invalid_page_id() {
        let bad = "\
---
page_id: not-a-valid-key
entity_id: milk-tea:drink:x
title: X
entity_type: drink
---

body";
        assert!(parse_page(bad).is_err());
    }

    #[test]
    fn no_heading_whole_body_becomes_overview() {
        let page = parse_page(
            "\
---
page_id: milk-tea:drink:x
entity_id: milk-tea:drink:x
title: X
entity_type: drink
---

纯文本导语，没有二级标题。",
        )
        .unwrap();
        assert_eq!(page.sections.len(), 1);
        assert_eq!(page.sections[0].heading, "概述");
        assert_eq!(page.sections[0].content, "纯文本导语，没有二级标题。");
    }

    #[test]
    fn h3_stays_inside_h2_section() {
        let page = parse_page(
            "\
---
page_id: milk-tea:drink:x
entity_id: milk-tea:drink:x
title: X
entity_type: drink
---

## 做法

### 冰度

去冰、少冰可选。
",
        )
        .unwrap();
        assert_eq!(page.sections.len(), 1);
        assert_eq!(page.sections[0].heading, "做法");
        assert!(page.sections[0].content.contains("### 冰度"));
    }
}
