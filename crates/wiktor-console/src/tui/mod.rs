//! TUI console（STEP12 B1/D1）：ratatui + crossterm 终端监督界面——四 Tab
//! （仪表盘/任务/审阅/查询），数据面与 Web console 同源（`state::assemble`），
//! 只读；本模块只做终端事件循环与渲染，状态转换全部在 `state`。
//! The TUI console (STEP12 B1/D1): a ratatui + crossterm terminal supervisory
//! surface — four tabs (dashboard/tasks/reviews/query) with the same data plane
//! as the Web console (`state::assemble`), read-only. This module only does the
//! terminal event loop and rendering; all state transitions live in `state`.

pub mod state;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table, Tabs};
use ratatui::{Frame, Terminal};
use wiktor_core::kernel::MockVectorStore;
use wiktor_core::kernel::SqliteKernel;
use wiktor_core::query_engine::QueryEngine;

use state::TuiState;

/// 轮询间隔（事件）与数据刷新周期。
/// The event poll interval and the data refresh period.
const POLL_MS: u64 = 250;
const REFRESH_SECS: u64 = 2;

/// 运行 TUI（阻塞直到退出；供 `wiktor tui` CLI 复用）。终端进入 raw mode +
/// alternate screen，退出时恢复；任何错误路径都先恢复终端再返回。
/// Runs the TUI (blocks until exit; shared by the `wiktor tui` CLI). The
/// terminal enters raw mode + the alternate screen and is restored on exit;
/// every error path restores the terminal before returning.
pub async fn run(db: &Path) -> anyhow::Result<()> {
    let (kernel, engines) = state::assemble(db).await?;
    let mut terminal = setup_terminal()?;
    let result = event_loop(&mut terminal, kernel, engines).await;
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> anyhow::Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(std::io::stdout());
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> anyhow::Result<()> {
    crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
    crossterm::terminal::disable_raw_mode()?;
    Ok(terminal.show_cursor()?)
}

/// 事件循环：每 REFRESH_SECS 装载一次数据（失败置 offline），每 POLL_MS 处理
/// 一次键盘输入。
/// The event loop: loads data every REFRESH_SECS (failures set offline) and
/// handles keyboard input every POLL_MS.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    kernel: Arc<SqliteKernel>,
    engines: std::collections::HashMap<String, Arc<QueryEngine<MockVectorStore>>>,
) -> anyhow::Result<()> {
    let mut app = TuiState::new();
    // Step14 P4：默认选中第一个已编译 domain（多域数据面；空库 → None）。
    // Step14 P4: select the first compiled domain by default (multi-domain data
    // plane; an empty DB → None).
    app.set_domain(engines.keys().next().cloned());
    let mut last_refresh = Instant::now() - Duration::from_secs(REFRESH_SECS);
    loop {
        if last_refresh.elapsed() >= Duration::from_secs(REFRESH_SECS) {
            match state::load_overview(&kernel) {
                Ok(data) => app.apply_overview(data),
                Err(_) => app.offline = true,
            }
            app.apply_tasks(state::load_tasks(&kernel));
            app.apply_reviews(state::load_reviews(&kernel));
            last_refresh = Instant::now();
        }

        terminal.draw(|f| render(f, &app))?;

        if !event::poll(Duration::from_millis(POLL_MS))? {
            continue;
        }
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match (key.code, key.modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
                    return Ok(())
                }
                (KeyCode::Char('1'), _) => app.select_tab(state::Tab::Overview),
                (KeyCode::Char('2'), _) => app.select_tab(state::Tab::Tasks),
                (KeyCode::Char('3'), _) => app.select_tab(state::Tab::Reviews),
                (KeyCode::Char('4'), _) => app.select_tab(state::Tab::Query),
                (KeyCode::Tab, _) | (KeyCode::Right, _) => app.cycle_tab(),
                (KeyCode::Backspace, _) => app.backspace(),
                (KeyCode::Enter, _)
                    if app.tab == state::Tab::Query && !app.query_input.is_empty() =>
                {
                    let input = app.query_input.clone();
                    // Step14 P4：按当前 domain 取 engine；缺省/未命中 → 第一个
                    // engine；无 engine → 空结果。
                    // Step14 P4: take the engine for the current domain; a
                    // missing/unmatched domain falls back to the first engine;
                    // no engine → an empty result.
                    let engine = app
                        .current_domain
                        .as_deref()
                        .and_then(|d| engines.get(d))
                        .or_else(|| engines.values().next())
                        .cloned();
                    let outcome = match engine {
                        Some(engine) => {
                            state::run_search(&engine, &input, 5, app.current_domain.as_deref())
                                .await
                        }
                        None => state::SearchOutcome::empty(&input),
                    };
                    app.apply_search(outcome);
                }
                (KeyCode::Char(c), _) => app.push_char(c),
                _ => {}
            }
        }
    }
}

/// 渲染一帧（D3：只读 state；TestBackend 测试直接调用本函数）。
/// Renders one frame (D3: reads only the state; TestBackend tests call this
/// directly).
pub fn render(f: &mut Frame, app: &TuiState) {
    let [title_area, tabs_area, body_area, help_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let offline = if app.offline { " · offline" } else { "" };
    let title = Line::from(vec![
        Span::styled(" Wiktor", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(" · 只读监督 TUI{}", offline),
            Style::default().fg(Color::Gray),
        ),
    ]);
    f.render_widget(title, title_area);

    let tabs = Tabs::new(state::TABS.iter().map(|t| t.title().to_string()))
        .select(state::TABS.iter().position(|t| *t == app.tab).unwrap_or(0))
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, tabs_area);

    match app.tab {
        state::Tab::Overview => render_overview(f, app, body_area),
        state::Tab::Tasks => render_tasks(f, app, body_area),
        state::Tab::Reviews => render_reviews(f, app, body_area),
        state::Tab::Query => render_query(f, app, body_area),
    }

    let help = Line::from(Span::styled(
        " 1-4/Tab 切换 · 查询页 Enter 检索 · q 退出 | 1-4/Tab switch · Enter search (query) · q quit",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(help, help_area);
}

fn render_overview(f: &mut Frame, app: &TuiState, area: ratatui::layout::Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" 仪表盘 Overview ");
    match &app.overview {
        None => {
            f.render_widget(block, area);
        }
        Some(o) => {
            let mut lines = vec![
                Line::from(format!(" schema_version: {}", o.schema_version)),
                Line::from(format!(
                    " review_pending: {}   generations: {}   published_pages: {}",
                    o.review_pending, o.generations, o.published_pages
                )),
                Line::from(""),
                Line::from(Span::styled(
                    " row_counts:",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
            ];
            for (name, count) in &o.rows {
                lines.push(Line::from(format!("   {:<22} {}", name, count)));
            }
            f.render_widget(Paragraph::new(lines).block(block), area);
        }
    }
}

fn render_tasks(f: &mut Frame, app: &TuiState, area: ratatui::layout::Rect) {
    let header = Row::new(["task_id", "entity_id", "status", "attempts"]);
    let rows: Vec<Row> = app
        .tasks
        .iter()
        .map(|t| {
            Row::new([
                t.task_id.to_string(),
                t.entity_id.clone(),
                t.status.clone(),
                t.attempt_count.to_string(),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Min(12),
            Constraint::Length(12),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" due 编译任务 Compile tasks "),
    );
    f.render_widget(table, area);
}

fn render_reviews(f: &mut Frame, app: &TuiState, area: ratatui::layout::Rect) {
    let header = Row::new(["review_id", "domain", "action", "status", "created_at"]);
    let rows: Vec<Row> = app
        .reviews
        .iter()
        .map(|r| {
            Row::new([
                r.review_id.to_string(),
                r.domain.clone(),
                r.action.clone(),
                r.status.clone(),
                r.created_at.to_string(),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(20),
            Constraint::Length(10),
            Constraint::Length(12),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" 审阅队列 Review queue "),
    );
    f.render_widget(table, area);
}

fn render_query(f: &mut Frame, app: &TuiState, area: ratatui::layout::Rect) {
    let [input, result] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    let input_widget = Paragraph::new(format!("> {}", app.query_input)).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" 检索 Search（Enter 执行） "),
    );
    f.render_widget(input_widget, input);

    let result_block = Block::default()
        .borders(Borders::ALL)
        .title(" 结果 Results ");
    match &app.search {
        None => f.render_widget(result_block, result),
        Some(s) => {
            if let Some(err) = &s.error {
                let line = Line::from(Span::styled(
                    format!(" error: {err}"),
                    Style::default().fg(Color::Red),
                ));
                f.render_widget(Paragraph::new(vec![line]).block(result_block), result);
                return;
            }
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    " rewrite={}  fts={}  vector={}  rrf_k={}  latency={}ms",
                    s.rewrite_status, s.fts_count, s.vector_count, s.rrf_k, s.latency_ms
                ),
                Style::default().fg(Color::Cyan),
            ))];
            if s.hits.is_empty() {
                lines.push(Line::from(" （无命中 no hits）"));
            } else {
                for h in &s.hits {
                    lines.push(Line::from(format!(
                        " {:<28} {:<20} {:.4}",
                        truncate(&h.title, 28),
                        truncate(&h.entity_id, 20),
                        h.score
                    )));
                }
            }
            f.render_widget(Paragraph::new(lines).block(result_block), result);
        }
    }
}

/// 定宽截断（CJK 宽字符按 char 数粗截，避免表格越界）。
/// Truncates to a width (CJK wide chars are coarsely cut by char count to keep
/// the table inside bounds).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// 把 TestBackend 缓冲区拍平成字符串（测试断言用）。
/// Flattens a TestBackend buffer into a string (for test assertions).
#[cfg(test)]
fn buffer_to_string(buffer: &ratatui::buffer::Buffer) -> String {
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// CJK 宽字符在缓冲区里夹着占位空格，断言一律比较去空白文本。
/// Wide CJK glyphs carry placeholder spaces in the buffer, so assertions
/// always compare whitespace-stripped text.
#[cfg(test)]
fn plain(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

#[cfg(test)]
fn assert_screen(screen: &str, needles: &[&str]) {
    let flat = plain(screen);
    for needle in needles {
        assert!(
            flat.contains(&plain(needle)),
            "missing {needle:?} in:\n{screen}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use state::{HitRow, OverviewData, ReviewRow, SearchOutcome, Tab, TaskRow};

    fn draw(state: &TuiState, w: u16, h: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, state)).unwrap();
        buffer_to_string(terminal.backend().buffer())
    }

    #[test]
    fn tabs_cycle_through_four_views() {
        let mut s = TuiState::new();
        assert_eq!(s.tab, Tab::Overview);
        s.cycle_tab();
        assert_eq!(s.tab, Tab::Tasks);
        s.cycle_tab();
        assert_eq!(s.tab, Tab::Reviews);
        s.cycle_tab();
        assert_eq!(s.tab, Tab::Query);
        s.cycle_tab();
        assert_eq!(s.tab, Tab::Overview);
    }

    #[test]
    fn query_input_only_collects_on_query_tab() {
        let mut s = TuiState::new();
        s.push_char('r');
        assert!(s.query_input.is_empty());
        s.select_tab(Tab::Query);
        for c in "rust".chars() {
            s.push_char(c);
        }
        assert_eq!(s.query_input, "rust");
        s.backspace();
        assert_eq!(s.query_input, "rus");
    }

    #[test]
    fn apply_clears_offline_and_fills_panels() {
        let mut s = TuiState::new();
        s.offline = true;
        s.apply_overview(OverviewData {
            schema_version: 6,
            ..Default::default()
        });
        assert!(!s.offline);
        assert_eq!(s.overview.as_ref().unwrap().schema_version, 6);
        s.apply_tasks(vec![TaskRow {
            task_id: 1,
            entity_id: "drink_a".into(),
            status: "succeeded".into(),
            attempt_count: 1,
        }]);
        s.apply_reviews(vec![ReviewRow {
            review_id: 2,
            domain: "milk-tea".into(),
            action: "supplemental_compile".into(),
            status: "Pending".into(),
            created_at: 42,
        }]);
        s.apply_search(SearchOutcome {
            query: "rust".into(),
            hits: vec![HitRow {
                title: "Rust async".into(),
                entity_id: "doc/1".into(),
                score: 0.5,
            }],
            rewrite_status: "Disabled".into(),
            fts_count: 3,
            vector_count: 0,
            rrf_k: 60,
            latency_ms: 1,
            error: None,
        });
        assert_eq!(s.tasks.len(), 1);
        assert_eq!(s.reviews.len(), 1);
        assert_eq!(s.search.as_ref().unwrap().fts_count, 3);
    }

    #[test]
    fn render_overview_shows_schema_and_counts() {
        let mut s = TuiState::new();
        s.apply_overview(OverviewData {
            schema_version: 6,
            rows: vec![("pages".into(), 20), ("facts".into(), 840)],
            review_pending: 2,
            generations: 1,
            published_pages: 20,
        });
        let screen = draw(&s, 60, 14);
        assert_screen(
            &screen,
            &["Wiktor", "仪表盘", "schema_version: 6", "pages", "840"],
        );
    }

    #[test]
    fn render_all_tabs_draw_with_empty_state() {
        for tab in state::TABS {
            let mut empty = TuiState::new();
            empty.select_tab(tab);
            let screen = draw(&empty, 60, 12);
            assert_screen(&screen, &[tab.title()]);
        }
    }

    #[test]
    fn render_query_shows_diagnostics_line() {
        let mut s = TuiState::new();
        s.select_tab(Tab::Query);
        s.push_char('r');
        s.apply_search(SearchOutcome {
            query: "r".into(),
            hits: vec![],
            rewrite_status: "Disabled".into(),
            fts_count: 0,
            vector_count: 0,
            rrf_k: 60,
            latency_ms: 1,
            error: None,
        });
        let screen = draw(&s, 70, 12);
        assert_screen(
            &screen,
            &["> r", "rewrite=Disabled", "fts=0", "rrf_k=60", "无命中"],
        );
    }

    #[test]
    fn render_query_shows_error_without_hits() {
        let mut s = TuiState::new();
        s.select_tab(Tab::Query);
        s.apply_search(SearchOutcome {
            query: "x".into(),
            hits: vec![],
            rewrite_status: "-".into(),
            fts_count: 0,
            vector_count: 0,
            rrf_k: 0,
            latency_ms: 0,
            error: Some("boom".into()),
        });
        let screen = draw(&s, 60, 10);
        assert_screen(&screen, &["error: boom"]);
    }

    #[tokio::test]
    async fn loads_real_empty_kernel_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let (kernel, engines) = state::assemble(&dir.path().join("tui.db")).await.unwrap();
        let overview = state::load_overview(&kernel).expect("overview loads");
        assert!(overview.rows.iter().any(|(n, _)| n == "pages"));
        assert!(state::load_tasks(&kernel).is_empty());
        assert!(state::load_reviews(&kernel).is_empty());
        // 空库无 engine；run_search 直接走空结果（Step14 P4 无域占位）。
        // An empty DB has no engine; run_search returns the no-domain placeholder
        // (Step14 P4).
        assert!(engines.is_empty());
        let outcome = state::SearchOutcome::empty("anything");
        assert!(outcome.error.is_some());
    }
}
