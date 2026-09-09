//! The palette: saved statements, filtered as you type, and the form that
//! fills one in.
//!
//! Two stages in one overlay rather than two overlays. Picking a template and
//! answering its placeholders are one errand — nobody opens the palette in
//! order to look at a list — and a second dialog on top of the first would put
//! two `Esc`s between somebody and the buffer they were aiming at.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use sqlake_core::library::{Template, TemplateId};
use sqlake_core::template::{Kind, Placeholder};

use crate::chrome::fit;
use crate::grid::sanitise;
use crate::hit::{HitMap, Target, Z_BACKDROP, Z_MODAL};

/// What the palette is showing.
///
/// The whole state travels on the `ViewCmd` that changes it, for the same
/// reason the explorer's filter does: the key that changed it is the only
/// thing that knows what it did, and a `Backspace` that removed nothing is not
/// a state the view should have to work out again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub filter: String,
    /// Which row is selected, counted over the *filtered* list.
    pub selected: usize,
    /// The template that was picked, and its unanswered placeholders. `None`
    /// while the list is still being chosen from.
    pub asking: Option<Form>,
}

impl Palette {
    /// Open and empty, as `Ctrl-p` leaves it.
    #[must_use]
    pub fn opening() -> Self {
        Self {
            filter: String::new(),
            selected: 0,
            asking: None,
        }
    }

    /// The templates this filter matches, in the order the file listed them.
    ///
    /// Case-insensitive over the name, and a substring rather than a prefix: a
    /// palette that only matches from the start is one where `orders` never
    /// finds `daily orders`.
    #[must_use]
    pub fn matching<'a>(&self, templates: &'a [Template]) -> Vec<&'a Template> {
        let wanted = self.filter.to_lowercase();
        templates
            .iter()
            .filter(|t| wanted.is_empty() || t.name.to_lowercase().contains(&wanted))
            .collect()
    }

    /// The template the selection is on, if the list is not empty.
    #[must_use]
    pub fn picked<'a>(&self, templates: &'a [Template]) -> Option<&'a Template> {
        self.matching(templates).get(self.selected).copied()
    }

    /// Move the selection, clamped to what is on the list.
    ///
    /// Clamped rather than wrapped, for the same reason the section list is: a
    /// list you can see all of should not jump back to the top when you step
    /// off the end.
    pub fn move_selection(&mut self, delta: i32, matches: usize) {
        let last = matches.saturating_sub(1);
        self.selected = self
            .selected
            .saturating_add_signed(delta as isize)
            .min(last);
    }
}

/// One placeholder, waiting for an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    pub value: String,
}

/// The picked template's placeholders, and where the keyboard is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub template: TemplateId,
    /// Kept for the title: the list is filtered underneath and the name would
    /// otherwise have to be looked up again to say what is being filled in.
    pub name: String,
    pub fields: Vec<Field>,
    pub at: usize,
    /// Why the last attempt to bind was refused — a template whose body cannot
    /// be filled in at all, which is a thing only the binding knows.
    pub failed: Option<String>,
}

impl Form {
    /// A form for these placeholders, with `filled` used where it has an
    /// answer already.
    ///
    /// `filled` is what the screen already knows — the relation the explorer
    /// has selected, for `{{ident:table}}`. Prefilled rather than inserted
    /// silently: it is still shown, and still editable, because the guess is
    /// only ever a guess about what somebody meant.
    #[must_use]
    pub fn new(
        template: TemplateId,
        name: String,
        placeholders: Vec<Placeholder>,
        filled: &dyn Fn(&Placeholder) -> Option<String>,
    ) -> Self {
        Self {
            template,
            name,
            fields: placeholders
                .into_iter()
                .map(|placeholder| Field {
                    value: filled(&placeholder).unwrap_or_default(),
                    name: placeholder.name,
                    kind: placeholder.kind,
                })
                .collect(),
            at: 0,
            failed: None,
        }
    }

    /// What to hand [`BoundTemplate::bind`](sqlake_core::template::BoundTemplate::bind).
    #[must_use]
    pub fn values(&self) -> std::collections::BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|f| (f.name.clone(), f.value.clone()))
            .collect()
    }

    pub fn edit(&mut self, value: String) {
        if let Some(field) = self.fields.get_mut(self.at) {
            field.value = value;
        }
    }

    /// Move between fields, clamped at both ends.
    pub fn move_to(&mut self, delta: i32) {
        self.at = self
            .at
            .saturating_add_signed(delta as isize)
            .min(self.fields.len().saturating_sub(1));
    }
}

/// How wide the overlay is, as a fraction of the screen.
const WIDTH_PERMILLE: u32 = 600;
/// And the most rows of list it will show, so a long list scrolls inside the
/// palette rather than turning it into the whole screen.
const MAX_ROWS: u16 = 12;

/// Draw the palette over everything, and register what can be clicked.
pub fn render(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    palette: &Palette,
    templates: &[Template],
    unavailable: Option<&str>,
) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    // Everything behind it dismisses it, the way a modal's backdrop does.
    hits.push(area, Z_BACKDROP, Target::Backdrop);

    let matches = palette.matching(templates);
    let rows = match &palette.asking {
        Some(form) => u16::try_from(form.fields.len()).unwrap_or(MAX_ROWS),
        None => u16::try_from(matches.len()).unwrap_or(MAX_ROWS),
    }
    .clamp(1, MAX_ROWS);

    let width = u16::try_from(u32::from(area.width) * WIDTH_PERMILLE / 1000)
        .unwrap_or(area.width)
        .clamp(20, area.width);
    // The filter line, the rows, and a border.
    let height = (rows + 4).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 3,
        width,
        height,
    };
    frame.render_widget(Clear, rect);
    // Its own body swallows clicks, so pressing the border does not dismiss it
    // through the backdrop underneath.
    hits.push(rect, Z_MODAL, Target::Modal);

    let title = match &palette.asking {
        Some(form) => format!(" {} ", form.name),
        None => " saved statements ".to_owned(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    match &palette.asking {
        Some(form) => form_lines(frame, inner, form),
        None => list(frame, hits, inner, palette, &matches, unavailable),
    }
}

fn list(
    frame: &mut Frame<'_>,
    hits: &mut HitMap,
    area: Rect,
    palette: &Palette,
    matches: &[&Template],
    unavailable: Option<&str>,
) {
    let prompt = Line::from(vec![
        Span::styled("> ", Style::default().fg(Color::Cyan)),
        Span::raw(sanitise(&palette.filter)),
        // A block rather than a real cursor: the terminal's own is where the
        // grid's selected cell is, and moving it here would take it off the
        // thing the palette is about to act on.
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ]);
    frame.render_widget(Paragraph::new(prompt), Rect { height: 1, ..area });

    let body = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    if body.height == 0 {
        return;
    }

    if let Some(why) = unavailable {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit(&sanitise(why), body.width),
                Style::default().fg(Color::Red),
            ))),
            body,
        );
        return;
    }
    if matches.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit("nothing saved matches", body.width),
                Style::default().fg(Color::DarkGray),
            ))),
            body,
        );
        return;
    }

    // The window follows the selection, so a list longer than the palette can
    // still be walked to the end.
    let visible = body.height as usize;
    let first = palette.selected.saturating_sub(visible.saturating_sub(1));
    for (row, template) in matches.iter().skip(first).take(visible).enumerate() {
        let index = first + row;
        let line = Rect {
            y: body.y + u16::try_from(row).unwrap_or(0),
            height: 1,
            ..body
        };
        hits.push(line, Z_MODAL, Target::PaletteRow { index });

        let selected = index == palette.selected;
        let style = if selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default()
        };
        let driver = template
            .driver
            .map_or_else(String::new, |kind| format!("  {}", kind.as_str()));
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit(&sanitise(&format!("{}{driver}", template.name)), line.width),
                style,
            ))),
            line,
        );
    }
}

fn form_lines(frame: &mut Frame<'_>, area: Rect, form: &Form) {
    for (index, field) in form.fields.iter().enumerate() {
        let Ok(offset) = u16::try_from(index) else {
            break;
        };
        if offset >= area.height {
            break;
        }
        let line = Rect {
            y: area.y + offset,
            height: 1,
            ..area
        };
        let here = index == form.at;
        let label = match field.kind {
            // Said, because it decides how the answer is quoted and therefore
            // what a person should type: a name here, a value there.
            Kind::Ident => format!("{} (name)", field.name),
            Kind::Value => field.name.clone(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{}{label}: ", if here { "> " } else { "  " }),
                    Style::default().fg(if here { Color::Cyan } else { Color::DarkGray }),
                ),
                Span::raw(sanitise(&field.value)),
                Span::styled(
                    if here { "▏" } else { "" },
                    Style::default().fg(Color::Cyan),
                ),
            ])),
            line,
        );
    }

    if let Some(why) = &form.failed
        && area.height > 0
    {
        let last = Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                fit(&sanitise(why), last.width),
                Style::default().fg(Color::Red),
            ))),
            last,
        );
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use sqlake_core::capability::DriverKind;
    use time::OffsetDateTime;

    use super::*;

    fn template(name: &str) -> Template {
        Template {
            id: TemplateId::new(1),
            name: name.to_owned(),
            body: "select 1".to_owned(),
            driver: None,
            tags: Vec::new(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn drawn(palette: &Palette, templates: &[Template], unavailable: Option<&str>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &mut hits,
                    frame.area(),
                    palette,
                    templates,
                    unavailable,
                );
            })
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn a_filter_matches_anywhere_in_the_name() {
        // A palette that only matches from the start is one where `orders`
        // never finds `daily orders`.
        let templates = [template("daily orders"), template("users by day")];
        let palette = Palette {
            filter: "orders".to_owned(),
            ..Palette::opening()
        };
        let names: Vec<&str> = palette
            .matching(&templates)
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, ["daily orders"]);
    }

    #[test]
    fn the_filter_ignores_case() {
        let templates = [template("Daily Orders")];
        let palette = Palette {
            filter: "daily".to_owned(),
            ..Palette::opening()
        };
        assert_eq!(palette.matching(&templates).len(), 1);
    }

    #[test]
    fn the_selection_stops_at_the_ends() {
        let templates = [template("one"), template("two")];
        let mut palette = Palette::opening();
        palette.move_selection(-1, templates.len());
        assert_eq!(palette.selected, 0, "it should not wrap to the bottom");
        palette.move_selection(10, templates.len());
        assert_eq!(palette.selected, 1, "nor past the end");
    }

    #[test]
    fn an_empty_list_leaves_the_selection_where_it_can_be_drawn() {
        let mut palette = Palette::opening();
        palette.move_selection(5, 0);
        assert_eq!(palette.selected, 0);
        assert_eq!(palette.picked(&[]), None);
    }

    #[test]
    fn every_row_is_something_a_pointer_can_reach() {
        let templates = [template("one"), template("two")];
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    &mut hits,
                    frame.area(),
                    &Palette::opening(),
                    &templates,
                    None,
                );
            })
            .unwrap();
        let rows: Vec<usize> = (0..16)
            .flat_map(|y| (0..60).map(move |x| (x, y)))
            .filter_map(
                |(x, y)| match hits.at(ratatui::layout::Position::new(x, y)) {
                    Some(Target::PaletteRow { index }) => Some(index),
                    _ => None,
                },
            )
            .collect();
        assert!(rows.contains(&0) && rows.contains(&1), "{rows:?}");
    }

    #[test]
    fn a_session_keeping_nothing_says_so_where_the_list_would_be() {
        let screen = drawn(&Palette::opening(), &[], Some("not keeping anything"));
        assert!(screen.contains("not keeping anything"), "{screen}");
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so() {
        let palette = Palette {
            filter: "zzz".to_owned(),
            ..Palette::opening()
        };
        let screen = drawn(&palette, &[template("one")], None);
        assert!(screen.contains("nothing saved matches"), "{screen}");
    }

    #[test]
    fn a_form_says_which_answers_are_names() {
        // The kind decides the quoting, so it decides what somebody should
        // type — and that is worth saying rather than leaving to be discovered
        // by the statement coming out wrong.
        let form = Form::new(
            TemplateId::new(1),
            "daily".to_owned(),
            vec![
                Placeholder {
                    name: "table".to_owned(),
                    kind: Kind::Ident,
                },
                Placeholder {
                    name: "since".to_owned(),
                    kind: Kind::Value,
                },
            ],
            &|_| None,
        );
        let screen = drawn(
            &Palette {
                asking: Some(form),
                ..Palette::opening()
            },
            &[],
            None,
        );
        assert!(screen.contains("table (name)"), "{screen}");
        assert!(screen.contains("since:"), "{screen}");
        assert!(
            screen.contains("daily"),
            "the title says what is being filled in"
        );
    }

    #[test]
    fn a_form_starts_on_what_the_screen_already_knows() {
        let form = Form::new(
            TemplateId::new(1),
            "daily".to_owned(),
            vec![Placeholder {
                name: "table".to_owned(),
                kind: Kind::Ident,
            }],
            &|p| (p.name == "table").then(|| "public.users".to_owned()),
        );
        assert_eq!(form.fields[0].value, "public.users");
        assert_eq!(form.values()["table"], "public.users");
    }

    #[test]
    fn a_driver_a_template_is_for_is_shown_beside_it() {
        let mut only_pg = template("pg only");
        only_pg.driver = Some(DriverKind::Postgres);
        let screen = drawn(&Palette::opening(), &[only_pg], None);
        assert!(screen.contains("postgres"), "{screen}");
    }
}
