#![windows_subsystem = "windows"]

use chrono::{DateTime, Duration as ChronoDuration, Local, NaiveDate, NaiveTime, TimeZone};
use eframe::egui::{self, Color32, RichText};
use egui_plot::{HLine, Line, Plot, PlotPoints, Points};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

const ORANGE: Color32 = Color32::from_rgb(255, 140, 0);
const LIGHT_ORANGE: Color32 = Color32::from_rgb(255, 232, 204);
const WHITE: Color32 = Color32::WHITE;

const LOW_COLOR: Color32 = Color32::from_rgb(211, 47, 47);
const NORMAL_COLOR: Color32 = Color32::from_rgb(76, 175, 80);
const HIGH_COLOR: Color32 = ORANGE;

const LOW_LIMIT: f64 = 3.9;
const HIGH_LIMIT: f64 = 10.0;

const ZERO_BELOW: f64 = 7.0;
const TARGET: f64 = 6.0;
const DIA_MIN: f64 = 240.0;
const INPUT_WINDOW_MIN: i64 = 120;
const ADD_DOSE_WINDOW_MIN: i64 = 12 * 60;
const MAX_UNITS: f64 = 100.0;
const INSULIN_KEEP_SEC: i64 = 7 * 24 * 3600;
const CAL_INPUT_WINDOW_MIN: i64 = 60;
const CAL_MATCH_MIN: i64 = 10;
const CAL_LAST_N: usize = 5;
const CAL_MAX_AGE_SEC: i64 = 24 * 3600;
const MAX_CAL: f64 = 3.0;
const REFRESH_SECS: u64 = 60;
const HISTORY_FULL: u32 = 2016;
const HISTORY_LIGHT: u32 = 36;
const HISTORY_KEEP_MS: i64 = 7 * 24 * 3600 * 1000;
const FULL_RELOAD_SECS: u64 = 1800;

#[derive(Serialize, Deserialize, Clone)]
struct CalPair {
    ts: i64,
    meter: f64,
    cgm: f64,
}

#[derive(Serialize, Deserialize, Clone)]
struct InsulinEntry {
    id: u64,
    ts: i64,
    units: f64,
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    url: String,
    isf: f64,
    #[serde(default)]
    accepted: bool,
    #[serde(default = "default_true")]
    use_cal: bool,
    #[serde(default)]
    cal: Vec<CalPair>,
    #[serde(default)]
    insulin_log: Vec<InsulinEntry>,
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            url: String::new(),
            isf: 0.0,
            accepted: false,
            use_cal: true,
            cal: Vec::new(),
            insulin_log: Vec::new(),
        }
    }
}

fn settings_path() -> std::path::PathBuf {
    let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    let dir = std::path::PathBuf::from(base).join("juno");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("settings.json")
}

fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_settings(s: &Settings) {
    if let Ok(text) = serde_json::to_string(s) {
        let _ = std::fs::write(settings_path(), text);
    }
}

#[derive(Clone)]
struct Reading {
    mmol: f64,
    trend: &'static str,
    ts_ms: i64,
}

struct Fetched {
    latest: Reading,
    history: Vec<(i64, f64)>,
}

fn fetch(url: &str, count: u32) -> Result<Fetched, String> {
    let base = url.trim().trim_end_matches('/');
    let base = if base.starts_with("http") {
        base.to_string()
    } else {
        format!("https://{}", base)
    };
    let resp = ureq::get(&format!("{}/api/v1/entries.json?count={}", base, count))
        .timeout(Duration::from_secs(30))
        .call()
        .map_err(|e| format!("Request error: {}", e))?;
    let v: serde_json::Value = resp
        .into_json()
        .map_err(|e| format!("Invalid response: {}", e))?;
    let arr = v.as_array().ok_or("Invalid response format")?;

    let mut history = Vec::new();
    for e in arr {
        let sgv = e.get("sgv").and_then(|x| x.as_f64());
        let date = e.get("date").and_then(|x| x.as_i64());
        if let (Some(sgv), Some(date)) = (sgv, date) {
            history.push((date, sgv / 18.0));
        }
    }
    let first = arr.first().ok_or("No data from Nightscout")?;
    let (ts_ms, mmol) = *history.first().ok_or("No glucose values in response")?;
    let trend = match first.get("direction").and_then(|d| d.as_str()).unwrap_or("") {
        "DoubleUp" => "↑↑",
        "SingleUp" => "↑",
        "FortyFiveUp" => "↗",
        "Flat" => "→",
        "FortyFiveDown" => "↘",
        "SingleDown" => "↓",
        "DoubleDown" => "↓↓",
        _ => "",
    };
    Ok(Fetched {
        latest: Reading {
            mmol,
            trend,
            ts_ms,
        },
        history,
    })
}

#[derive(Clone)]
struct Dose {
    time: DateTime<Local>,
    units: f64,
    log_id: Option<u64>,
}

fn active_iob(doses: &[Dose]) -> f64 {
    let now = Local::now();
    doses
        .iter()
        .map(|d| {
            let mins = (now - d.time).num_seconds() as f64 / 60.0;
            let remaining = (1.0 - mins / DIA_MIN).clamp(0.0, 1.0);
            d.units * remaining
        })
        .sum()
}

struct CalcResult {
    correction: f64,
    iob: f64,
    dose: f64,
}

fn calculate(bg: f64, isf: f64, iob: f64) -> CalcResult {
    if bg < ZERO_BELOW || isf <= 0.0 {
        return CalcResult {
            correction: 0.0,
            iob,
            dose: 0.0,
        };
    }
    let correction = (bg - TARGET) / isf;
    let dose = (correction - iob).max(0.0);
    CalcResult {
        correction,
        iob,
        dose: (dose * 2.0).round() / 2.0,
    }
}

fn parse_time_within(text: &str, max_min: i64) -> Result<DateTime<Local>, String> {
    let t = NaiveTime::parse_from_str(&text.trim().replace('.', ":"), "%H:%M")
        .map_err(|_| "Enter time as HH:MM, e.g. 14:30".to_string())?;
    let now = Local::now();
    let naive = now.date_naive().and_time(t);
    let mut dt = Local
        .from_local_datetime(&naive)
        .single()
        .ok_or("Could not parse time")?;
    if dt > now {
        dt = dt - ChronoDuration::days(1);
    }
    if (now - dt).num_minutes() > max_min {
        let span = if max_min % 60 == 0 {
            format!("{} h", max_min / 60)
        } else {
            format!("{} min", max_min)
        };
        return Err(format!("Time must be within the last {}", span));
    }
    Ok(dt)
}

fn fmt_ts(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

fn fmt_dt(ts: i64) -> String {
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|t| t.format("%m.%d %H:%M").to_string())
        .unwrap_or_default()
}

fn fmt_ts_ms(ts_ms: i64) -> String {
    fmt_ts(ts_ms.div_euclid(1000))
}

fn day_of(ts: i64) -> Option<NaiveDate> {
    Local.timestamp_opt(ts, 0).single().map(|t| t.date_naive())
}

struct Stats {
    low: usize,
    normal: usize,
    high: usize,
    avg: f64,
    min: f64,
    max: f64,
}

impl Stats {
    fn total(&self) -> usize {
        self.low + self.normal + self.high
    }
}

fn tab_row<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    current: &mut T,
    items: &[(T, &str)],
    width: f32,
) {
    ui.horizontal(|ui| {
        for &(value, label) in items {
            let selected = *current == value;
            let btn = if selected {
                egui::Button::new(RichText::new(label).color(WHITE).strong().size(15.0))
                    .fill(ORANGE)
            } else {
                egui::Button::new(RichText::new(label).color(ORANGE).size(15.0)).fill(WHITE)
            }
            .min_size(egui::vec2(width, 34.0));
            if ui.add(btn).clicked() {
                *current = value;
            }
        }
    });
}

fn draw_pie(ui: &mut egui::Ui, slices: &[(f64, Color32)], radius: f32, avg_text: &str) {
    let size = egui::vec2(radius * 2.0 + 8.0, radius * 2.0 + 8.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let center = rect.center();
    let total: f64 = slices.iter().map(|s| s.0).sum();

    if total > 0.0 {
        let nonzero = slices.iter().filter(|s| s.0 > 0.0).count();
        let mut start = -std::f32::consts::FRAC_PI_2;
        for &(val, color) in slices {
            if val <= 0.0 {
                continue;
            }
            let sweep = (val / total) as f32 * std::f32::consts::TAU;
            let steps = ((sweep / std::f32::consts::TAU) * 120.0).ceil().max(2.0) as usize;
            let mut mesh = egui::Mesh::default();
            mesh.colored_vertex(center, color);
            for i in 0..=steps {
                let a = start + sweep * (i as f32) / (steps as f32);
                mesh.colored_vertex(center + egui::vec2(a.cos(), a.sin()) * radius, color);
            }
            for i in 0..steps {
                mesh.add_triangle(0, (i + 1) as u32, (i + 2) as u32);
            }
            painter.add(egui::Shape::mesh(mesh));
            if nonzero > 1 {
                let d = egui::vec2(start.cos(), start.sin()) * radius;
                painter.line_segment([center, center + d], egui::Stroke::new(2.0_f32, WHITE));
            }
            start += sweep;
        }
    } else {
        painter.circle_filled(center, radius, Color32::from_gray(225));
    }

    painter.circle_filled(center, radius * 0.55, WHITE);
    painter.text(
        center - egui::vec2(0.0, 9.0),
        egui::Align2::CENTER_CENTER,
        avg_text,
        egui::FontId::proportional(28.0),
        Color32::from_rgb(40, 40, 40),
    );
    painter.text(
        center + egui::vec2(0.0, 15.0),
        egui::Align2::CENTER_CENTER,
        "average, mmol/L",
        egui::FontId::proportional(12.0),
        Color32::GRAY,
    );
}

#[derive(PartialEq, Clone, Copy)]
enum Screen {
    Disclaimer,
    Setup,
    Main,
    AddDose,
    Report,
    Address,
}

#[derive(PartialEq, Clone, Copy)]
enum CalcTab {
    Raw,
    Ready,
}

#[derive(PartialEq, Clone, Copy)]
enum ReportTab {
    Sugar,
    Insulin,
}

struct App {
    screen: Screen,
    settings: Settings,
    agree_checked: bool,
    url_input: String,
    isf_input: String,
    setup_error: String,

    reading: Option<Reading>,
    history: Vec<(i64, f64)>,
    chart_hours: u32,
    report_hours: u32,
    report_tab: ReportTab,
    fetch_error: String,
    loading: bool,
    pending_full: bool,
    full_loaded: bool,
    last_full: Instant,
    last_fetch: Instant,
    tx: Sender<Result<Fetched, String>>,
    rx: Receiver<Result<Fetched, String>>,

    show_calc: bool,
    calc_tab: CalcTab,
    doses: Vec<Dose>,
    time_input: String,
    units_input: String,
    dose_error: String,
    show_picker: bool,
    picker_sel: HashSet<u64>,

    add_time_input: String,
    add_units_input: String,
    add_to_calc: bool,
    add_error: String,
    add_msg: String,

    show_cal: bool,
    cal_value_input: String,
    cal_time_input: String,
    cal_error: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut v = egui::Visuals::light();
        v.panel_fill = WHITE;
        v.window_fill = WHITE;
        v.selection.bg_fill = ORANGE;
        v.widgets.hovered.bg_fill = LIGHT_ORANGE;
        v.widgets.hovered.weak_bg_fill = LIGHT_ORANGE;
        v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0_f32, ORANGE);
        v.widgets.hovered.bg_stroke = egui::Stroke::new(1.5f32, ORANGE);
        cc.egui_ctx.set_visuals(v);

        let (tx, rx) = channel();
        let settings = load_settings();
        let screen = if settings.accepted {
            Screen::Setup
        } else {
            Screen::Disclaimer
        };

        let now_ts = Local::now().timestamp();
        let doses: Vec<Dose> = settings
            .insulin_log
            .iter()
            .filter(|e| ((now_ts - e.ts) as f64) / 60.0 < DIA_MIN)
            .filter_map(|e| {
                Local.timestamp_opt(e.ts, 0).single().map(|t| Dose {
                    time: t,
                    units: e.units,
                    log_id: Some(e.id),
                })
            })
            .collect();

        App {
            screen,
            url_input: settings.url.clone(),
            isf_input: if settings.isf > 0.0 {
                settings.isf.to_string()
            } else {
                String::new()
            },
            settings,
            agree_checked: false,
            setup_error: String::new(),
            reading: None,
            history: Vec::new(),
            chart_hours: 6,
            report_hours: 24,
            report_tab: ReportTab::Sugar,
            fetch_error: String::new(),
            loading: false,
            pending_full: false,
            full_loaded: false,
            last_full: Instant::now(),
            last_fetch: Instant::now(),
            tx,
            rx,
            show_calc: false,
            calc_tab: CalcTab::Ready,
            doses,
            time_input: String::new(),
            units_input: String::new(),
            dose_error: String::new(),
            show_picker: false,
            picker_sel: HashSet::new(),
            add_time_input: String::new(),
            add_units_input: String::new(),
            add_to_calc: true,
            add_error: String::new(),
            add_msg: String::new(),
            show_cal: false,
            cal_value_input: String::new(),
            cal_time_input: String::new(),
            cal_error: String::new(),
        }
    }

    fn start_fetch(&mut self, ctx: &egui::Context, force_full: bool) {
        if self.loading {
            return;
        }
        let full = force_full
            || !self.full_loaded
            || self.last_full.elapsed() > Duration::from_secs(FULL_RELOAD_SECS);
        self.pending_full = full;
        self.loading = true;
        self.last_fetch = Instant::now();
        let count = if full { HISTORY_FULL } else { HISTORY_LIGHT };
        let url = self.settings.url.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(fetch(&url, count));
            ctx.request_repaint();
        });
    }

    fn merge_history(&mut self, new: Vec<(i64, f64)>) {
        let mut map: BTreeMap<i64, f64> = self.history.iter().cloned().collect();
        for (t, v) in new {
            map.insert(t, v);
        }
        let cutoff = Local::now().timestamp_millis() - HISTORY_KEEP_MS;
        self.history = map.into_iter().rev().filter(|(t, _)| *t >= cutoff).collect();
    }

    fn orange_button(text: &str) -> egui::Button<'static> {
        egui::Button::new(RichText::new(text.to_string()).color(WHITE).size(16.0))
            .fill(ORANGE)
            .min_size(egui::vec2(160.0, 36.0))
    }

    fn cal_offset(&self) -> Option<f64> {
        if self.settings.cal.is_empty() {
            return None;
        }
        let mut v: Vec<&CalPair> = self.settings.cal.iter().collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.ts));
        v.truncate(CAL_LAST_N);
        let sum: f64 = v.iter().map(|p| p.meter - p.cgm).sum();
        Some(sum / v.len() as f64)
    }

    fn applied_offset(&self) -> f64 {
        if !self.settings.use_cal {
            return 0.0;
        }
        self.cal_offset().unwrap_or(0.0).clamp(-MAX_CAL, MAX_CAL)
    }

    fn corrected(&self, raw: f64) -> f64 {
        (raw + self.applied_offset()).max(0.0)
    }

    fn nearest_cgm(&self, dt: DateTime<Local>) -> Option<f64> {
        let target = dt.timestamp_millis();
        self.history
            .iter()
            .min_by_key(|(ts, _)| (ts - target).abs())
            .filter(|(ts, _)| (ts - target).abs() <= CAL_MATCH_MIN * 60_000)
            .map(|&(_, v)| v)
    }

    fn current_bg(&self) -> Option<f64> {
        self.reading.as_ref().map(|r| self.corrected(r.mmol))
    }

    fn current_calc(&self) -> Option<CalcResult> {
        let bg = self.current_bg()?;
        Some(calculate(bg, self.settings.isf, active_iob(&self.doses)))
    }

    fn report_stats(&self, hours: u32) -> Option<Stats> {
        let cutoff = Local::now().timestamp_millis() - hours as i64 * 3_600_000;
        let vals: Vec<f64> = self
            .history
            .iter()
            .filter(|(ts, _)| *ts >= cutoff)
            .map(|&(_, raw)| self.corrected(raw))
            .collect();
        if vals.is_empty() {
            return None;
        }
        let low = vals.iter().filter(|&&v| v < LOW_LIMIT).count();
        let high = vals.iter().filter(|&&v| v > HIGH_LIMIT).count();
        let normal = vals.len() - low - high;
        let avg = vals.iter().sum::<f64>() / vals.len() as f64;
        let min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        Some(Stats {
            low,
            normal,
            high,
            avg,
            min,
            max,
        })
    }

    fn apply_picker(&mut self) {
        self.doses.retain(|d| d.log_id.is_none());
        for e in &self.settings.insulin_log {
            if self.picker_sel.contains(&e.id) {
                if let Some(t) = Local.timestamp_opt(e.ts, 0).single() {
                    self.doses.push(Dose {
                        time: t,
                        units: e.units,
                        log_id: Some(e.id),
                    });
                }
            }
        }
        self.doses.sort_by_key(|d| d.time);
        self.show_picker = false;
    }

    fn try_add_dose(&mut self) -> Result<String, String> {
        let units: f64 = self
            .add_units_input
            .trim()
            .replace(',', ".")
            .parse()
            .unwrap_or(0.0);
        if !(units > 0.0 && units <= MAX_UNITS) {
            return Err(format!(
                "Enter units: a number greater than 0, at most {:.0}",
                MAX_UNITS
            ));
        }
        let dt = parse_time_within(&self.add_time_input, ADD_DOSE_WINDOW_MIN)?;
        let id = self
            .settings
            .insulin_log
            .iter()
            .map(|e| e.id)
            .max()
            .unwrap_or(0)
            + 1;
        self.settings.insulin_log.push(InsulinEntry {
            id,
            ts: dt.timestamp(),
            units,
        });
        self.settings.insulin_log.sort_by_key(|e| e.ts);
        save_settings(&self.settings);

        let age_min = (Local::now() - dt).num_seconds() as f64 / 60.0;
        if self.add_to_calc && age_min < DIA_MIN {
            self.doses.push(Dose {
                time: dt,
                units,
                log_id: Some(id),
            });
            self.doses.sort_by_key(|d| d.time);
        }
        self.add_units_input.clear();
        Ok(format!(
            "Added: {} — {:.1} U",
            dt.format("%m.%d %H:%M"),
            units
        ))
    }

    fn disclaimer_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.label(
            RichText::new("Important information")
                .size(28.0)
                .color(ORANGE)
                .strong(),
        );
        ui.add_space(16.0);
        ui.label(
            RichText::new(
                "This app makes a rough, approximate calculation of an insulin dose. \
                 The result may be inaccurate: it does not account for food, physical activity, \
                 illness, or other factors.\n\n\
                 This app is not a substitute for a doctor. The user bears full responsibility \
                 for any decision about an insulin dose and for any consequences.",
            )
            .size(16.0),
        );
        ui.add_space(20.0);
        ui.checkbox(
            &mut self.agree_checked,
            "I understand this and accept responsibility",
        );
        ui.add_space(16.0);
        if ui
            .add_enabled(self.agree_checked, Self::orange_button("Continue"))
            .clicked()
        {
            self.settings.accepted = true;
            save_settings(&self.settings);
            self.screen = Screen::Setup;
        }
    }

    fn address_form(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, title: &str, button: &str) {
        ui.add_space(20.0);
        ui.label(RichText::new(title).size(32.0).color(ORANGE).strong());
        ui.add_space(20.0);

        ui.label("Nightscout URL:");
        ui.add(
            egui::TextEdit::singleline(&mut self.url_input)
                .hint_text("https://my-site.herokuapp.com")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(12.0);

        ui.label("Insulin sensitivity (mmol/L per 1 U):");
        ui.add(
            egui::TextEdit::singleline(&mut self.isf_input)
                .hint_text("e.g. 2.5")
                .desired_width(f32::INFINITY),
        );
        ui.add_space(16.0);

        if ui.add(Self::orange_button(button)).clicked() {
            let isf: f64 = self.isf_input.trim().replace(',', ".").parse().unwrap_or(0.0);
            let new_url = self.url_input.trim().to_string();
            if new_url.is_empty() {
                self.setup_error = "Enter the Nightscout URL".into();
            } else if isf <= 0.0 {
                self.setup_error = "Enter insulin sensitivity — a number greater than 0".into();
            } else {
                if new_url != self.settings.url {
                    self.reading = None;
                    self.history.clear();
                    self.settings.cal.clear();
                    self.full_loaded = false;
                }
                self.settings.url = new_url;
                self.settings.isf = isf;
                save_settings(&self.settings);
                self.setup_error.clear();
                self.fetch_error.clear();
                self.screen = Screen::Main;
                self.start_fetch(ctx, true);
            }
        }
        if !self.setup_error.is_empty() {
            ui.add_space(8.0);
            ui.colored_label(Color32::RED, &self.setup_error);
        }
    }

    fn nav_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 3.0 * ui.spacing().item_spacing.x) / 4.0;
            for (screen, label) in [
                (Screen::Main, "Home"),
                (Screen::AddDose, "+ Dose"),
                (Screen::Report, "Report"),
                (Screen::Address, "Address"),
            ] {
                let selected = self.screen == screen;
                let btn = if selected {
                    egui::Button::new(RichText::new(label).color(WHITE).strong().size(15.0))
                        .fill(ORANGE)
                } else {
                    egui::Button::new(RichText::new(label).color(ORANGE).size(15.0)).fill(WHITE)
                }
                .min_size(egui::vec2(w, 38.0));
                if ui.add(btn).clicked() && !selected {
                    self.screen = screen;
                    self.show_calc = false;
                    self.show_picker = false;
                    self.show_cal = false;
                    match screen {
                        Screen::Address => {
                            self.url_input = self.settings.url.clone();
                            self.isf_input = self.settings.isf.to_string();
                            self.setup_error.clear();
                        }
                        Screen::AddDose => {
                            self.add_time_input = Local::now().format("%H:%M").to_string();
                            self.add_error.clear();
                            self.add_msg.clear();
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    fn main_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let offset = self.applied_offset();
        let reading = self.reading.clone();
        ui.horizontal(|ui| match &reading {
            Some(r) => {
                let bg = self.corrected(r.mmol);
                let color = if bg >= ZERO_BELOW {
                    Color32::from_rgb(220, 90, 0)
                } else {
                    Color32::from_rgb(40, 40, 40)
                };
                ui.label(
                    RichText::new(format!("{:.1}", bg))
                        .size(64.0)
                        .strong()
                        .color(color),
                );
                ui.vertical(|ui| {
                    ui.add_space(14.0);
                    ui.label(RichText::new(r.trend).size(28.0).color(ORANGE));
                    ui.label(RichText::new("mmol/L").color(Color32::GRAY));
                });
            }
            None => {
                if self.loading {
                    ui.label(RichText::new("Loading…").size(28.0).color(ORANGE));
                } else {
                    ui.label(RichText::new("—").size(64.0).color(Color32::GRAY));
                }
            }
        });

        if let Some(r) = &reading {
            let age_min = ((Local::now().timestamp_millis() - r.ts_ms) / 60_000).max(0);
            let age_text = if age_min >= 60 {
                format!("{} h {} min ago", age_min / 60, age_min % 60)
            } else {
                format!("{} min ago", age_min)
            };
            let (txt, col) = if age_min > 15 {
                (
                    format!(
                        "Latest reading: {} ({}) — data is stale",
                        fmt_ts_ms(r.ts_ms),
                        age_text
                    ),
                    Color32::RED,
                )
            } else {
                (
                    format!("Latest reading: {} ({})", fmt_ts_ms(r.ts_ms), age_text),
                    Color32::from_rgb(90, 90, 90),
                )
            };
            ui.label(RichText::new(txt).size(14.0).color(col));
            if offset.abs() > 0.0 {
                ui.label(
                    RichText::new(format!(
                        "Sensor {:.1}, meter correction {:+.1}",
                        r.mmol, offset
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
            }
        }

        if !self.fetch_error.is_empty() {
            ui.colored_label(Color32::RED, &self.fetch_error);
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);

        if let Some(c) = self.current_calc() {
            ui.label(RichText::new("How much to inject:").size(18.0));
            ui.label(
                RichText::new(format!("{:.1} U", c.dose))
                    .size(40.0)
                    .strong()
                    .color(ORANGE),
            );
            ui.label(
                RichText::new(format!(
                    "Active insulin on board: ≈{:.2} U  (doses counted: {})",
                    c.iob,
                    self.doses.len()
                ))
                .color(Color32::GRAY),
            );
            ui.add_space(10.0);
        }

        ui.horizontal(|ui| {
            if ui.add(Self::orange_button("How much to inject")).clicked() {
                self.show_calc = true;
                self.show_picker = false;
                self.dose_error.clear();
                self.time_input = Local::now().format("%H:%M").to_string();
            }
            if ui.add(Self::orange_button("Calibration")).clicked() {
                self.show_cal = true;
                self.cal_error.clear();
                self.cal_time_input = Local::now().format("%H:%M").to_string();
            }
        });

        ui.add_space(10.0);
        if ui.button("Refresh").clicked() {
            self.start_fetch(ctx, true);
        }

        if self.show_calc {
            self.calc_window(ctx);
        }
        if self.show_cal {
            self.cal_window(ctx);
        }
    }

    fn add_dose_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(
            RichText::new("Add dose")
                .size(28.0)
                .color(ORANGE)
                .strong(),
        );
        ui.add_space(12.0);

        ui.label("Dose time:");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.add_time_input)
                    .hint_text("HH:MM")
                    .desired_width(70.0),
            );
            if ui.small_button("Now").clicked() {
                self.add_time_input = Local::now().format("%H:%M").to_string();
            }
        });
        ui.label(
            RichText::new(format!(
                "You can log a dose from the last {} h",
                ADD_DOSE_WINDOW_MIN / 60
            ))
            .small()
            .color(Color32::GRAY),
        );

        ui.add_space(8.0);
        ui.label("Insulin amount, U:");
        ui.add(
            egui::TextEdit::singleline(&mut self.add_units_input)
                .hint_text("0")
                .desired_width(70.0),
        );

        ui.add_space(8.0);
        ui.checkbox(
            &mut self.add_to_calc,
            "Include in the “How much to inject” calculation",
        );
        ui.add_space(10.0);

        if ui.add(Self::orange_button("Add")).clicked() {
            match self.try_add_dose() {
                Ok(m) => {
                    self.add_msg = m;
                    self.add_error.clear();
                }
                Err(e) => {
                    self.add_error = e;
                    self.add_msg.clear();
                }
            }
        }
        if !self.add_error.is_empty() {
            ui.add_space(6.0);
            ui.colored_label(Color32::RED, &self.add_error);
        }
        if !self.add_msg.is_empty() {
            ui.add_space(6.0);
            ui.colored_label(NORMAL_COLOR, &self.add_msg);
        }

        ui.add_space(14.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(RichText::new("Recent entries:").strong());
        if self.settings.insulin_log.is_empty() {
            ui.label(RichText::new("none yet").color(Color32::GRAY));
        }
        for e in self.settings.insulin_log.iter().rev().take(5) {
            ui.label(format!("{}   —   {:.1} U", fmt_dt(e.ts), e.units));
        }
    }

    fn report_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.label(RichText::new("Report").size(28.0).color(ORANGE).strong());
        ui.add_space(6.0);

        let w = (ui.available_width() - ui.spacing().item_spacing.x) / 2.0;
        tab_row(
            ui,
            &mut self.report_tab,
            &[
                (ReportTab::Sugar, "Glucose"),
                (ReportTab::Insulin, "Insulin"),
            ],
            w,
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for (h, label) in [(24u32, "24 hours"), (72, "3 days"), (168, "7 days")] {
                ui.selectable_value(&mut self.report_hours, h, label);
            }
        });
        ui.add_space(10.0);

        match self.report_tab {
            ReportTab::Sugar => self.report_sugar_ui(ui),
            ReportTab::Insulin => self.report_insulin_ui(ui),
        }
    }

    fn report_sugar_ui(&mut self, ui: &mut egui::Ui) {
        match self.report_stats(self.report_hours) {
            None => {
                ui.label(RichText::new("No data for the selected period").color(Color32::GRAY));
            }
            Some(s) => {
                let total = s.total() as f64;
                ui.vertical_centered(|ui| {
                    draw_pie(
                        ui,
                        &[
                            (s.high as f64, HIGH_COLOR),
                            (s.normal as f64, NORMAL_COLOR),
                            (s.low as f64, LOW_COLOR),
                        ],
                        95.0,
                        &format!("{:.1}", s.avg),
                    );
                });
                ui.add_space(14.0);

                let rows = [
                    (format!("High (above {:.0})", HIGH_LIMIT), s.high, HIGH_COLOR),
                    (
                        format!("In range ({:.1}–{:.0})", LOW_LIMIT, HIGH_LIMIT),
                        s.normal,
                        NORMAL_COLOR,
                    ),
                    (format!("Low (below {:.1})", LOW_LIMIT), s.low, LOW_COLOR),
                ];
                for (label, count, color) in rows {
                    ui.horizontal(|ui| {
                        let (r, _) =
                            ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                        ui.painter().rect_filled(r, 4.0, color);
                        ui.label(
                            RichText::new(format!(
                                "{}: {:.0}%  ({} readings)",
                                label,
                                count as f64 / total * 100.0,
                                count
                            ))
                            .size(15.0),
                        );
                    });
                }

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);
                ui.label(format!(
                    "Average glucose: {:.1} mmol/L   Min: {:.1}   Max: {:.1}",
                    s.avg, s.min, s.max
                ));
                ui.label(
                    RichText::new(format!(
                        "Readings in period: {}{}",
                        s.total(),
                        if self.applied_offset().abs() > 0.0 {
                            ", calibration applied"
                        } else {
                            ""
                        }
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
            }
        }
    }

    fn report_insulin_ui(&mut self, ui: &mut egui::Ui) {
        let cutoff = Local::now().timestamp() - self.report_hours as i64 * 3600;
        let mut entries: Vec<InsulinEntry> = self
            .settings
            .insulin_log
            .iter()
            .filter(|e| e.ts >= cutoff)
            .cloned()
            .collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.ts));

        if entries.is_empty() {
            ui.label(
                RichText::new("No doses for the selected period. Add them in the “+ Dose” tab.")
                    .color(Color32::GRAY),
            );
            return;
        }

        let total: f64 = entries.iter().map(|e| e.units).sum();
        ui.label(
            RichText::new(format!("Total: {:.1} U   Doses: {}", total, entries.len()))
                .size(16.0)
                .strong(),
        );
        ui.add_space(6.0);

        let mut remove_id: Option<u64> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(ui.available_height())
            .show(ui, |ui| {
                let mut last_day: Option<NaiveDate> = None;
                for e in &entries {
                    let day = day_of(e.ts);
                    if day != last_day {
                        last_day = day;
                        let day_total: f64 = entries
                            .iter()
                            .filter(|x| day_of(x.ts) == day)
                            .map(|x| x.units)
                            .sum();
                        let label = day
                            .map(|d| d.format("%m.%d.%Y").to_string())
                            .unwrap_or_default();
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(format!("{} — total {:.1} U", label, day_total))
                                .color(ORANGE)
                                .strong(),
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.label(format!("{}   —   {:.1} U", fmt_ts(e.ts), e.units));
                        if ui.small_button("Delete").clicked() {
                            remove_id = Some(e.id);
                        }
                    });
                }
            });

        if let Some(id) = remove_id {
            self.settings.insulin_log.retain(|e| e.id != id);
            self.doses.retain(|d| d.log_id != Some(id));
            self.picker_sel.remove(&id);
            save_settings(&self.settings);
        }
    }

    fn chart_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Glucose chart").strong().color(ORANGE));
            for h in [3u32, 6, 12, 24] {
                ui.selectable_value(&mut self.chart_hours, h, format!("{} h", h));
            }
        });

        let now_ms = Local::now().timestamp_millis();
        let hours = self.chart_hours as f64;

        let pts: Vec<[f64; 2]> = self
            .history
            .iter()
            .rev()
            .filter_map(|&(ts, raw)| {
                let x = -((now_ms - ts) as f64) / 3_600_000.0;
                if x < -hours {
                    None
                } else {
                    Some([x, self.corrected(raw)])
                }
            })
            .collect();

        let marks: Vec<[f64; 2]> = self
            .settings
            .cal
            .iter()
            .filter_map(|p| {
                let x = -((now_ms - p.ts * 1000) as f64) / 3_600_000.0;
                if x < -hours {
                    None
                } else {
                    Some([x, p.meter])
                }
            })
            .collect();

        Plot::new("bg_plot")
            .height(190.0)
            .allow_drag(false)
            .allow_zoom(false)
            .allow_scroll(false)
            .allow_boxed_zoom(false)
            .include_y(2.0)
            .include_y(14.0)
            .include_x(-hours)
            .include_x(0.0)
            .x_axis_label("hours ago")
            .y_axis_label("mmol/L")
            .show(ui, |plot_ui| {
                plot_ui.hline(HLine::new(LOW_LIMIT).color(LOW_COLOR));
                plot_ui.hline(HLine::new(HIGH_LIMIT).color(Color32::from_rgb(255, 190, 110)));
                plot_ui.line(Line::new(PlotPoints::from(pts)).color(ORANGE).width(2.5_f32));
                if !marks.is_empty() {
                    plot_ui.points(
                        Points::new(PlotPoints::from(marks))
                            .radius(5.0_f32)
                            .color(Color32::BLACK)
                            .name("Meter"),
                    );
                }
            });
        ui.label(
            RichText::new("Black dots — meter readings. Lines: 3.9 and 10 mmol/L.")
                .small()
                .color(Color32::GRAY),
        );
    }

    fn calc_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut remove: Option<usize> = None;
        let mut do_apply = false;
        let mut do_cancel = false;

        let bg = self.current_bg();
        let isf = self.settings.isf;
        let ready = self.current_calc();
        let raw = bg.map(|b| calculate(b, isf, 0.0));

        let recent: Vec<InsulinEntry> = {
            let now = Local::now().timestamp();
            let mut v: Vec<InsulinEntry> = self
                .settings
                .insulin_log
                .iter()
                .filter(|e| ((now - e.ts) as f64) / 60.0 < DIA_MIN)
                .cloned()
                .collect();
            v.sort_by_key(|e| std::cmp::Reverse(e.ts));
            v
        };

        egui::Window::new(RichText::new("How much to inject").color(ORANGE))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(340.0);
                tab_row(
                    ui,
                    &mut self.calc_tab,
                    &[
                        (CalcTab::Raw, "Raw value"),
                        (CalcTab::Ready, "Ready value"),
                    ],
                    165.0,
                );
                ui.add_space(8.0);

                match self.calc_tab {
                    CalcTab::Raw => {
                        ui.label(
                            RichText::new(
                                "Calculation without insulin you have already taken.",
                            )
                            .small()
                            .color(Color32::GRAY),
                        );
                        ui.add_space(6.0);
                        match (&raw, bg) {
                            (Some(c), Some(b)) => {
                                ui.label(format!(
                                    "Glucose: {:.1} mmol/L, target: {:.1}\n\
                                     Correction needed: {:.2} U",
                                    b, TARGET, c.correction
                                ));
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(format!("Raw value: {:.1} U", c.dose))
                                        .size(22.0)
                                        .strong()
                                        .color(ORANGE),
                                );
                            }
                            _ => {
                                ui.label("No glucose data");
                            }
                        }
                    }
                    CalcTab::Ready => {
                        ui.label("Doses counted in the calculation:");
                        ui.add_space(4.0);
                        if self.doses.is_empty() {
                            ui.label(RichText::new("none yet").color(Color32::GRAY));
                        }
                        for (i, d) in self.doses.iter().enumerate() {
                            ui.horizontal(|ui| {
                                let tag = if d.log_id.is_some() { "  (from log)" } else { "" };
                                ui.label(format!(
                                    "{}  —  {:.1} U{}",
                                    d.time.format("%H:%M"),
                                    d.units,
                                    tag
                                ));
                                if ui.small_button("Remove").clicked() {
                                    remove = Some(i);
                                }
                            });
                        }

                        ui.add_space(8.0);
                        if ui.add(Self::orange_button("Add from log")).clicked() {
                            self.show_picker = !self.show_picker;
                            if self.show_picker {
                                self.picker_sel =
                                    self.doses.iter().filter_map(|d| d.log_id).collect();
                            }
                        }

                        if self.show_picker {
                            ui.add_space(6.0);
                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                ui.label(format!(
                                    "Doses from the log (last {:.0} h):",
                                    DIA_MIN / 60.0
                                ));
                                if recent.is_empty() {
                                    ui.label(
                                        RichText::new(
                                            "No recent doses. Add them in the “+ Dose” tab.",
                                        )
                                        .color(Color32::GRAY),
                                    );
                                }
                                for e in &recent {
                                    let mut checked = self.picker_sel.contains(&e.id);
                                    if ui
                                        .checkbox(
                                            &mut checked,
                                            format!("{}  —  {:.1} U", fmt_ts(e.ts), e.units),
                                        )
                                        .changed()
                                    {
                                        if checked {
                                            self.picker_sel.insert(e.id);
                                        } else {
                                            self.picker_sel.remove(&e.id);
                                        }
                                    }
                                }
                                ui.add_space(4.0);
                                ui.horizontal(|ui| {
                                    if ui.add(Self::orange_button("Apply")).clicked() {
                                        do_apply = true;
                                    }
                                    if ui.button("Cancel").clicked() {
                                        do_cancel = true;
                                    }
                                });
                            });
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        ui.label("Add a dose manually:");
                        ui.horizontal(|ui| {
                            ui.label("Time");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.time_input)
                                    .hint_text("HH:MM")
                                    .desired_width(60.0),
                            );
                            if ui.small_button("Now").clicked() {
                                self.time_input = Local::now().format("%H:%M").to_string();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label("Units ");
                            ui.add(
                                egui::TextEdit::singleline(&mut self.units_input)
                                    .hint_text("0")
                                    .desired_width(60.0),
                            );
                        });
                        if ui.add(Self::orange_button("Add")).clicked() {
                            let units: f64 = self
                                .units_input
                                .trim()
                                .replace(',', ".")
                                .parse()
                                .unwrap_or(-1.0);
                            if !(units > 0.0 && units <= MAX_UNITS) {
                                self.dose_error = format!(
                                    "Enter units: a number greater than 0, at most {:.0}",
                                    MAX_UNITS
                                );
                            } else {
                                match parse_time_within(&self.time_input, INPUT_WINDOW_MIN) {
                                    Ok(time) => {
                                        self.doses.push(Dose {
                                            time,
                                            units,
                                            log_id: None,
                                        });
                                        self.doses.sort_by_key(|d| d.time);
                                        self.units_input.clear();
                                        self.dose_error.clear();
                                    }
                                    Err(e) => self.dose_error = e,
                                }
                            }
                        }
                        if !self.dose_error.is_empty() {
                            ui.colored_label(Color32::RED, &self.dose_error);
                        }

                        ui.add_space(8.0);
                        ui.separator();
                        if let Some(c) = &ready {
                            ui.label(format!(
                                "Correction needed: {:.2} U\nActive insulin: ≈{:.2} U",
                                c.correction, c.iob
                            ));
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(format!("Ready value: {:.1} U", c.dose))
                                    .size(22.0)
                                    .strong()
                                    .color(ORANGE),
                            );
                        } else {
                            ui.label("No glucose data");
                        }
                    }
                }

                ui.add_space(8.0);
                ui.label(
                    RichText::new("Rough calculation. You are responsible for the dose.")
                        .small()
                        .color(Color32::GRAY),
                );
            });

        if let Some(i) = remove {
            self.doses.remove(i);
        }
        if do_apply {
            self.apply_picker();
        }
        if do_cancel {
            self.show_picker = false;
        }
        if !open {
            self.show_calc = false;
            self.show_picker = false;
        }
    }

    fn cal_window(&mut self, ctx: &egui::Context) {
        let mut open = true;
        let mut remove: Option<usize> = None;
        let offset = self.cal_offset();

        egui::Window::new(RichText::new("Meter calibration").color(ORANGE))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_max_width(340.0);
                ui.label(
                    RichText::new(
                        "Measure your glucose with a meter and enter the reading. \
                         It is best to calibrate when glucose is stable (arrow →): \
                         during a rapid rise or fall the sensor lags behind blood.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
                ui.add_space(6.0);

                ui.label("Readings entered (last 24 hours):");
                if self.settings.cal.is_empty() {
                    ui.label(RichText::new("none yet").color(Color32::GRAY));
                }
                for (i, p) in self.settings.cal.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "{}  meter {:.1} / sensor {:.1}",
                            fmt_ts(p.ts),
                            p.meter,
                            p.cgm
                        ));
                        if ui.small_button("Delete").clicked() {
                            remove = Some(i);
                        }
                    });
                }

                ui.add_space(8.0);
                ui.separator();
                ui.label("Add a reading:");
                ui.horizontal(|ui| {
                    ui.label("Meter, mmol/L");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cal_value_input)
                            .hint_text("6.4")
                            .desired_width(60.0),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Time");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.cal_time_input)
                            .hint_text("HH:MM")
                            .desired_width(60.0),
                    );
                    if ui.small_button("Now").clicked() {
                        self.cal_time_input = Local::now().format("%H:%M").to_string();
                    }
                });
                if ui.add(Self::orange_button("Add")).clicked() {
                    let meter: f64 = self
                        .cal_value_input
                        .trim()
                        .replace(',', ".")
                        .parse()
                        .unwrap_or(0.0);
                    if !(1.0..=33.0).contains(&meter) {
                        self.cal_error =
                            "Enter a meter reading from 1 to 33 mmol/L".into();
                    } else {
                        match parse_time_within(&self.cal_time_input, CAL_INPUT_WINDOW_MIN) {
                            Ok(dt) => match self.nearest_cgm(dt) {
                                Some(cgm) => {
                                    self.settings.cal.push(CalPair {
                                        ts: dt.timestamp(),
                                        meter,
                                        cgm,
                                    });
                                    self.settings.cal.sort_by_key(|p| p.ts);
                                    save_settings(&self.settings);
                                    self.cal_value_input.clear();
                                    self.cal_error.clear();
                                }
                                None => {
                                    self.cal_error = format!(
                                        "No sensor data near this time (±{} min). \
                                         Tap “Refresh” and try again.",
                                        CAL_MATCH_MIN
                                    );
                                }
                            },
                            Err(e) => self.cal_error = e,
                        }
                    }
                }
                if !self.cal_error.is_empty() {
                    ui.colored_label(Color32::RED, &self.cal_error);
                }

                ui.add_space(8.0);
                ui.separator();
                if ui
                    .checkbox(&mut self.settings.use_cal, "Apply calibration")
                    .changed()
                {
                    save_settings(&self.settings);
                }
                match offset {
                    Some(o) => {
                        ui.label(format!(
                            "Average difference (meter − sensor): {:+.2} mmol/L",
                            o
                        ));
                        if o.abs() > MAX_CAL {
                            ui.colored_label(
                                Color32::RED,
                                format!(
                                    "Difference exceeds {:.0} mmol/L; at most ±{:.0} is applied. \
                                     Check the sensor and the meter.",
                                    MAX_CAL, MAX_CAL
                                ),
                            );
                        }
                    }
                    None => {
                        ui.label(RichText::new("Correction is not applied").color(Color32::GRAY));
                    }
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "Correction is the average of the last {} readings; readings and dose \
                         calculation use the corrected value.",
                        CAL_LAST_N
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
            });

        if let Some(i) = remove {
            self.settings.cal.remove(i);
            save_settings(&self.settings);
        }
        if !open {
            self.show_cal = false;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(res) = self.rx.try_recv() {
            self.loading = false;
            match res {
                Ok(f) => {
                    if self.pending_full {
                        self.full_loaded = true;
                        self.last_full = Instant::now();
                    }
                    self.reading = Some(f.latest);
                    self.merge_history(f.history);
                    self.fetch_error.clear();
                }
                Err(e) => self.fetch_error = e,
            }
        }

        let now = Local::now();

        self.doses
            .retain(|d| ((now - d.time).num_seconds() as f64 / 60.0) < DIA_MIN);

        let cutoff = now.timestamp() - CAL_MAX_AGE_SEC;
        let before = self.settings.cal.len();
        self.settings.cal.retain(|p| p.ts >= cutoff);
        if self.settings.cal.len() != before {
            save_settings(&self.settings);
        }

        let log_cutoff = now.timestamp() - INSULIN_KEEP_SEC;
        let before = self.settings.insulin_log.len();
        self.settings.insulin_log.retain(|e| e.ts >= log_cutoff);
        if self.settings.insulin_log.len() != before {
            save_settings(&self.settings);
        }

        let logged_in = matches!(
            self.screen,
            Screen::Main | Screen::AddDose | Screen::Report | Screen::Address
        );
        if logged_in
            && !self.loading
            && self.last_fetch.elapsed() >= Duration::from_secs(REFRESH_SECS)
        {
            self.start_fetch(ctx, false);
        }
        ctx.request_repaint_after(Duration::from_secs(1));

        egui::TopBottomPanel::top("bar")
            .exact_height(8.0)
            .frame(egui::Frame::none().fill(ORANGE))
            .show(ctx, |_| {});

        if logged_in {
            egui::TopBottomPanel::bottom("nav")
                .frame(egui::Frame::none().fill(WHITE).inner_margin(8.0))
                .show(ctx, |ui| self.nav_ui(ui));
        }

        if self.screen == Screen::Main {
            egui::TopBottomPanel::bottom("chart")
                .frame(egui::Frame::none().fill(WHITE).inner_margin(12.0))
                .show(ctx, |ui| self.chart_ui(ui));
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(WHITE).inner_margin(20.0))
            .show(ctx, |ui| match self.screen {
                Screen::Disclaimer => self.disclaimer_ui(ui),
                Screen::Setup => self.address_form(ui, ctx, "Nightscout", "Sign in"),
                Screen::Main => self.main_ui(ui, ctx),
                Screen::AddDose => self.add_dose_ui(ui),
                Screen::Report => self.report_ui(ui),
                Screen::Address => self.address_form(ui, ctx, "Nightscout address", "Save"),
            });
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 860.0])
            .with_min_inner_size([400.0, 740.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Juno",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
