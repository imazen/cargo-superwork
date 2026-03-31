use crate::config::SuperworkConfig;
use crate::discover;
use std::collections::BTreeMap;
use std::path::Path;

/// Generate the "Image tech I maintain" ecosystem links section.
/// If `highlight` is given, that crate is bolded in the output.
pub fn run(root: &Path, config: &SuperworkConfig, highlight: Option<&str>) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;

    // Build crate-to-github-url map
    let mut urls: BTreeMap<&str, String> = BTreeMap::new();
    for info in eco.crates.values() {
        if let Some(url) = config.github_url_for(&info.repo_dir) {
            urls.insert(&info.name, url);
        }
    }

    // Categorize crates
    let codecs: Vec<&str> = [
        "zenjpeg",
        "zenpng",
        "zenwebp",
        "zengif",
        "zenavif",
        "zenjxl",
        "zentiff",
        "zenbitmaps",
        "heic",
        "zenraw",
        "zenpdf",
        "ultrahdr",
        "jxl-encoder",
        "zenjxl-decoder",
        "rav1d-safe",
        "zenrav1e",
        "zenavif-parse",
        "zenavif-serialize",
        "mozjpeg-rs",
        "webpx",
    ]
    .into_iter()
    .filter(|n| urls.contains_key(n))
    .collect();

    let compression: Vec<&str> = ["zenflate", "zenzop", "zenzstd"]
        .into_iter()
        .filter(|n| urls.contains_key(n))
        .collect();

    let processing: Vec<&str> = ["zenresize", "zenfilters", "zenquant", "zenblend"]
        .into_iter()
        .filter(|n| urls.contains_key(n))
        .collect();

    let metrics: Vec<&str> = [
        "zensim",
        "fast-ssim2",
        "butteraugli",
        "resamplescope-rs",
        "codec-eval",
        "codec-corpus",
    ]
    .into_iter()
    .filter(|n| urls.contains_key(n))
    .collect();

    let pixels: Vec<&str> = ["zenpixels", "zenpixels-convert", "linear-srgb", "garb"]
        .into_iter()
        .filter(|n| urls.contains_key(n))
        .collect();

    let pipeline: Vec<&str> = ["zenpipe", "zencodec", "zencodecs", "zenlayout", "zennode"]
        .into_iter()
        .filter(|n| urls.contains_key(n))
        .collect();

    let infra: Vec<&str> = [
        "archmage",
        "magetypes",
        "enough",
        "whereat",
        "zenbench",
        "cargo-copter",
    ]
    .into_iter()
    .filter(|n| urls.contains_key(n))
    .collect();

    let fmt = |names: &[&str]| -> String {
        names
            .iter()
            .map(|n| {
                if highlight == Some(n) {
                    format!("**{n}**")
                } else {
                    format!("[{n}]")
                }
            })
            .collect::<Vec<_>>()
            .join(" · ")
    };

    // Print the table
    println!("## Image tech I maintain");
    println!();
    println!("| | |");
    println!("|:--|:--|");

    if !codecs.is_empty() {
        println!("| State of the art codecs | {} |", fmt(&codecs));
    }
    if !compression.is_empty() {
        println!("| Compression | {} |", fmt(&compression));
    }
    if !processing.is_empty() {
        println!("| Processing | {} |", fmt(&processing));
    }
    if !metrics.is_empty() {
        println!("| Metrics | {} |", fmt(&metrics));
    }
    if !pixels.is_empty() {
        println!("| Pixel types & color | {} |", fmt(&pixels));
    }
    if !pipeline.is_empty() {
        println!("| Pipeline | {} |", fmt(&pipeline));
    }

    println!("| ImageResizer | [ImageResizer] (C#) — 24M+ NuGet downloads across all packages |");
    println!(
        "| [Imageflow][] | Image optimization engine (Rust) — [.NET][imageflow-dotnet] · [node][imageflow-node] · [go][imageflow-go] — 9M+ NuGet downloads |"
    );
    println!(
        "| [Imageflow Server][] | [The fast, safe image server](https://www.imazen.io/) (Rust+C#) — 552K+ NuGet downloads |"
    );
    println!();

    if !infra.is_empty() {
        println!("### General Rust awesomeness");
        println!();
        println!("{}", fmt(&infra));
        println!();
    }

    println!(
        "[And other projects](https://www.imazen.io/open-source) · [GitHub @imazen](https://github.com/imazen) · [GitHub @lilith](https://github.com/lilith) · [lib.rs/~lilith](https://lib.rs/~lilith) · [NuGet](https://www.nuget.org/profiles/imazen) (over 30 million downloads / 87 packages)"
    );

    // Print link references
    println!();
    for (name, url) in &urls {
        println!("[{name}]: {url}");
    }
    // Static links not in the ecosystem
    println!("[Imageflow]: https://github.com/imazen/imageflow");
    println!("[Imageflow Server]: https://github.com/imazen/imageflow-server");
    println!("[imageflow-dotnet]: https://github.com/imazen/imageflow-dotnet");
    println!("[imageflow-node]: https://github.com/imazen/imageflow-node");
    println!("[imageflow-go]: https://github.com/imazen/imageflow-go");
    println!("[ImageResizer]: https://github.com/imazen/resizer");

    Ok(())
}
