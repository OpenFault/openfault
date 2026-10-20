//! Inline charts shared by the dashboard, issue list, issue detail and TV (templates/chart.html).
//! `{{ chart|safe }}` renders one; everything numeric is computed here, the template only lays it out.
use askama::Template;
use std::fmt;

pub struct Bar {
    /// height in % of the box
    pub h: f64,
    pub v: i64,
    pub tip: String,
}

pub struct Chart {
    /// "area" or "bars"
    pub kind: &'static str,
    pub id: String,
    /// box height in px; area charts stretch to their container's width
    pub h: u32,
    pub d: String,
    /// last point of an area chart, in % of the box (drives the glowing dot)
    pub x: f64,
    pub y: f64,
    pub bars: Vec<Bar>,
    pub label: String,
    /// bars only: CSS tooltips (big charts); tiny row charts skip them
    pub tips: bool,
}

#[derive(Template)]
#[template(path = "chart.html")]
struct One<'a> {
    c: &'a Chart,
}

impl fmt::Display for Chart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&One { c: self }.render().map_err(|_| fmt::Error)?)
    }
}

const W: f64 = 100.0;

/// Area sparkline: one path (monotone cubic line + gradient fill). The closing edges sit outside
/// the viewBox so only the curve gets stroked.
pub fn area(id: impl Into<String>, vals: &[i64], h: u32) -> Chart {
    let hf = h as f64;
    let max = vals.iter().copied().max().unwrap_or(0).max(1) as f64;
    let n = vals.len().max(2);
    let step = W / (n - 1) as f64;
    let pts: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let v = vals.get(i).copied().unwrap_or(0) as f64;
            (i as f64 * step, hf - 1.0 - v / max * (hf - 4.0))
        })
        .collect();
    let (x0, y0) = pts[0];
    let (xn, yn) = pts[n - 1];
    let d = format!(
        "M-2,{y0:.2}L{x0:.2},{y0:.2}{}L{:.2},{yn:.2}L{:.2},{:.2}L-2,{:.2}Z",
        monotone(&pts),
        W + 2.0,
        W + 2.0,
        hf + 2.0,
        hf + 2.0
    );
    Chart {
        kind: "area",
        id: id.into(),
        h,
        d,
        x: xn / W * 100.0,
        y: yn / hf * 100.0,
        bars: vec![],
        label: format!("{} events", vals.iter().sum::<i64>()),
        tips: false,
    }
}

/// Fritsch–Carlson monotone cubic through `p` as SVG `C` segments (no overshoot below zero).
fn monotone(p: &[(f64, f64)]) -> String {
    let n = p.len();
    let d: Vec<f64> = (0..n - 1)
        .map(|i| (p[i + 1].1 - p[i].1) / (p[i + 1].0 - p[i].0))
        .collect();
    let mut m = vec![0.0; n];
    m[0] = d[0];
    m[n - 1] = d[n - 2];
    for i in 1..n - 1 {
        m[i] = if d[i - 1] * d[i] <= 0.0 {
            0.0
        } else {
            (d[i - 1] + d[i]) / 2.0
        };
    }
    for i in 0..n - 1 {
        if d[i] == 0.0 {
            (m[i], m[i + 1]) = (0.0, 0.0);
        } else {
            let (a, b) = (m[i] / d[i], m[i + 1] / d[i]);
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                (m[i], m[i + 1]) = (t * a * d[i], t * b * d[i]);
            }
        }
    }
    (0..n - 1)
        .map(|i| {
            let ((x0, y0), (x1, y1)) = (p[i], p[i + 1]);
            let dx = (x1 - x0) / 3.0;
            format!(
                "C{:.2},{:.2} {:.2},{:.2} {x1:.2},{y1:.2}",
                x0 + dx,
                y0 + m[i] * dx,
                x1 - dx,
                y1 - m[i + 1] * dx
            )
        })
        .collect()
}

/// Bars; `labels[i]` names bucket i in the tooltip ("14:00").
pub fn bars(vals: &[i64], labels: &[String], h: u32, tips: bool) -> Chart {
    let max = vals.iter().copied().max().unwrap_or(0).max(1) as f64;
    let total: i64 = vals.iter().sum();
    Chart {
        kind: "bars",
        id: String::new(),
        h,
        d: String::new(),
        x: 0.0,
        y: 0.0,
        bars: vals
            .iter()
            .enumerate()
            .map(|(i, &v)| Bar {
                h: v as f64 / max * 100.0,
                v,
                tip: match labels.get(i) {
                    Some(l) => format!("{} · {l}", super::n(&v)),
                    None => super::n(&v),
                },
            })
            .collect(),
        label: format!("{} events", super::n(&total)),
        tips,
    }
}

/// "HH:MM" (sub-day buckets) or "Mon 02" labels for bucket start times.
pub fn labels(ts: &[i64], step: i64) -> Vec<String> {
    let f = if step < 86400 { "%H:%M" } else { "%b %d" };
    ts.iter()
        .map(|t| {
            jiff::Timestamp::from_second(*t)
                .map(|t| t.strftime(f).to_string())
                .unwrap_or_default()
        })
        .collect()
}
