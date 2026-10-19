//! Native CLI used to verify rendering, detection and export against the
//! sample document before hooking everything up to the GUI.

use std::time::Instant;
use std::path::PathBuf;

use cropper_core::plan::{plan_crops, CropMode, PlanOptions, TargetSize};
use cropper_core::session::Session;
use cropper_core::types::{DetectOptions, PaperSize};

struct Args {
    input: PathBuf,
    out: Option<PathBuf>,
    max_px: u32,
    threshold: u8,
    min_ink: u32,
    edge_mm: f32,
    pad_mm: f32,
    uniform: bool,
    paper: Option<PaperSize>,
    align: (f32, f32),
}

fn parse_args() -> Result<Args, String> {
    let mut raw = std::env::args().skip(1);
    let mut args = Args {
        input: PathBuf::from("sample/sample.pdf"),
        out: None,
        max_px: 1200,
        align: (0.5, 0.5),
        threshold: 200,
        min_ink: 3,
        edge_mm: 0.0,
        pad_mm: 3.0,
        uniform: false,
        paper: None,
    };
    while let Some(flag) = raw.next() {
        match flag.as_str() {
            "--out" => args.out = Some(PathBuf::from(raw.next().ok_or("--out 값 없음")?)),
            "--paper" => {
                let name = raw.next().ok_or("--paper 값 없음")?;
                args.paper = Some(
                    PaperSize::parse(&name).ok_or("--paper: A3|A4|A5|B4|B5|Letter|Legal")?,
                );
            }
            "--max-px" => args.max_px = raw.next().ok_or("--max-px 값 없음")?.parse().map_err(|_| "--max-px 숫자 아님")?,
            "--threshold" => args.threshold = raw.next().ok_or("--threshold 값 없음")?.parse().map_err(|_| "--threshold 숫자 아님")?,
            "--edge-mm" => args.edge_mm = raw.next().ok_or("--edge-mm 값 없음")?.parse().map_err(|_| "--edge-mm 숫자 아님")?,
            "--min-ink" => args.min_ink = raw.next().ok_or("--min-ink 값 없음")?.parse().map_err(|_| "--min-ink 숫자 아님")?,
            "--pad-mm" => args.pad_mm = raw.next().ok_or("--pad-mm 값 없음")?.parse().map_err(|_| "--pad-mm 숫자 아님")?,
            "--align" => {
                // two letters: horizontal l|c|r + vertical t|c|b, e.g. "rt" = top-right
                let v = raw.next().ok_or("--align 값 없음")?.to_lowercase();
                let b: Vec<char> = v.chars().collect();
                let h = match b.first() { Some('l') => 0.0, Some('c') => 0.5, Some('r') => 1.0, _ => return Err("--align: [l|c|r][t|c|b] (예: rt)".into()) };
                let t = match b.get(1) { Some('t') => 0.0, Some('c') => 0.5, Some('b') => 1.0, _ => return Err("--align: [l|c|r][t|c|b] (예: rt)".into()) };
                args.align = (h, t);
            }
            "--uniform" => args.uniform = true,
            other => args.input = PathBuf::from(other),
        }
    }
    Ok(args)
}

fn main() -> Result<(), String> {
    let args = parse_args()?;
    let source = std::fs::read(&args.input)
        .map_err(|e| format!("{} 읽기 실패: {e}", args.input.display()))?;

    let mut session = Session::new(source)?;
    let page_count = session.page_count();
    let dims: Vec<(f32, f32)> = (0..page_count)
        .map(|i| session.page_size(i))
        .collect::<Result<Vec<_>, _>>()?;
    let options = DetectOptions { threshold: args.threshold, min_ink: args.min_ink, edge_mm: args.edge_mm };

    println!("{:?}: {} pages", args.input, page_count);

    let mut detected = Vec::with_capacity(page_count);
    for index in 0..page_count {
        let start = Instant::now();
        let (crop, ratio, blank, _t) = session.analyze(index, args.max_px, &options)?;
        let elapsed = start.elapsed();
        if blank {
            println!("p{:02} {:>6}ms  BLANK (ink {:.4})", index + 1, elapsed.as_millis(), ratio);
        } else {
            let (pw, ph) = dims[index];
            println!(
                "p{:02} {:>6}ms  margins L{:.1} T{:.1} R{:.1} B{:.1} pt (ink {:.3})",
                index + 1,
                elapsed.as_millis(),
                crop.x,
                crop.y,
                pw - crop.right(),
                ph - crop.bottom(),
                ratio,
            );
        }
        detected.push(if blank { None } else { Some(crop) });
    }

    let enabled = vec![true; page_count];
    let target = match args.paper {
        None => TargetSize::Content,
        Some(paper) => {
            let (w, h) = paper.dims_pt();
            TargetSize::Paper { width: w, height: h }
        }
    };
    let plan_options = PlanOptions {
        pad_mm: args.pad_mm,
        mode: if args.uniform { CropMode::Uniform } else { CropMode::PerPage },
        target,
        align: args.align,
    };
    let final_crops = plan_crops(&dims, &detected, &enabled, &plan_options);

    if let Some(out) = &args.out {
        let start = Instant::now();
        let bytes = session.export(&final_crops)?;
        std::fs::write(out, &bytes).map_err(|e| format!("쓰기 실패: {e}"))?;
        let first = final_crops.iter().flatten().next();
        let size_note = match first {
            Some(p) => format!("{:.0}×{:.0} pt", p.page_width, p.page_height),
            None => "원본".to_owned(),
        };
        println!(
            "export: {} -> {} pages, output page {size_note} ({:.1} KiB) in {:.1} ms",
            out.display(),
            page_count,
            bytes.len() as f64 / 1024.0,
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
    Ok(())
}
