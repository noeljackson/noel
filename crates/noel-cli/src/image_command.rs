//! `noel image`: generating images with an image model.

use super::*;

impl Cli {
    pub(crate) fn owned_screen(&self) -> bool {
        match self.tui_mode {
            Some(mode) => mode == TuiMode::Fullscreen,
            None => !self.no_alt_screen,
        }
    }
}

pub(crate) async fn try_run_image_command() -> Result<bool> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(false);
    };
    match command {
        "images" | "image-models" => {
            run_image_model_list(&args[1..])?;
            Ok(true)
        }
        "generate-image" | "image-generate" => {
            run_image_generate(&args[1..]).await?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub(crate) fn run_image_model_list(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("usage: noel images [search]\n\nList image-generation models.");
        return Ok(());
    }
    let cwd = std::env::current_dir()?;
    let config = load_config(config_paths(cwd, None)?)?;
    let search = args.first().map(|value| value.to_ascii_lowercase());
    for model in &config.image_models {
        if let Some(search) = &search {
            let display = format!(
                "{}/{} {} {:?}",
                model.provider,
                model.id,
                model.name.as_deref().unwrap_or_default(),
                model.api
            )
            .to_ascii_lowercase();
            if !display.contains(search) {
                continue;
            }
        }
        println!("{}/{}\t{:?}", model.provider, model.id, model.api);
    }
    Ok(())
}

pub(crate) async fn run_image_generate(args: &[String]) -> Result<()> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        print_image_generate_help();
        return Ok(());
    }
    let cwd = std::env::current_dir()?;
    let mut model_ref = "openrouter/google/gemini-3.1-flash-image-preview".to_string();
    let mut output_path = None::<PathBuf>;
    let mut image_paths = Vec::<PathBuf>::new();
    let mut prompt_parts = Vec::<String>::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--model" | "-m" => {
                index += 1;
                model_ref = args
                    .get(index)
                    .ok_or_else(|| anyhow!("--model requires a value"))?
                    .clone();
            }
            "--output" | "-o" => {
                index += 1;
                output_path = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| anyhow!("--output requires a value"))?,
                ));
            }
            "--image" | "-i" => {
                index += 1;
                image_paths.push(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| anyhow!("--image requires a value"))?,
                ));
            }
            value => prompt_parts.push(value.to_string()),
        }
        index += 1;
    }
    let output_path = output_path.ok_or_else(|| anyhow!("--output is required"))?;
    let prompt = prompt_parts.join(" ");
    if prompt.trim().is_empty() {
        return Err(anyhow!("image generation requires a prompt"));
    }
    let config = load_config(config_paths(cwd.clone(), None)?)?;
    println!(
        "{}",
        generate_image_to_path(
            &config,
            &cwd,
            &model_ref,
            &output_path,
            &prompt,
            &image_paths
        )
        .await?
    );
    Ok(())
}

pub(crate) fn print_image_generate_help() {
    println!(
        "usage: noel generate-image --output <file> [--model <provider/model>] [--image <file>] <prompt>\n\nGenerate images through a configured image model and write output files locally."
    );
}

pub(crate) fn resolve_image_model_reference(
    config: &LoadedConfig,
    reference: &str,
) -> Option<ImageModelDefinition> {
    let reference = reference.trim();
    config
        .image_models
        .iter()
        .find(|model| {
            reference == model.id
                || reference == format!("{}/{}", model.provider, model.id)
                || model.name.as_deref() == Some(reference)
        })
        .cloned()
}

pub(crate) fn map_image_provider_api(api: &ConfigImageProviderApi) -> AiImageProviderApi {
    match api {
        ConfigImageProviderApi::OpenRouterImages => AiImageProviderApi::OpenRouterImages,
    }
}

pub(crate) async fn generate_image_to_path(
    config: &LoadedConfig,
    cwd: &Path,
    model_ref: &str,
    output_path: &Path,
    prompt: &str,
    image_paths: &[PathBuf],
) -> Result<String> {
    if images_blocked(config) {
        return Err(anyhow!("images are blocked by settings"));
    }
    let model = resolve_image_model_reference(config, model_ref)
        .ok_or_else(|| anyhow!("image model not found: {model_ref}"))?;
    let auth = auth_for_provider(&config.auth, &model.provider, None).ok_or_else(|| {
        anyhow!(
            "provider {} requires auth; set auth.json or {}",
            model.provider,
            default_api_key_env(&model.provider).unwrap_or("provider API key env")
        )
    })?;
    let mut input = vec![ImageGenerationInput::Text {
        text: prompt.to_string(),
    }];
    for media in load_media_inputs(cwd, image_paths, config)? {
        input.push(ImageGenerationInput::Image { media });
    }
    let result = generate_images(
        ImageProviderConfig {
            model: ModelRef {
                provider: model.provider.clone(),
                id: model.id.clone(),
            },
            api: map_image_provider_api(&model.api),
            base_url: model.base_url.clone(),
            auth: map_provider_auth(Some(auth)),
            output_modalities: model.output.clone(),
        },
        input,
    )
    .await?;
    let mut lines = write_generated_image_outputs(output_path, &result.output)?;
    if let Some(response_id) = result.response_id {
        lines.push(format!("response: {response_id}"));
    }
    lines.push(format!(
        "usage: input_tokens={} output_tokens={}",
        result.input_tokens, result.output_tokens
    ));
    Ok(lines.join("\n"))
}

pub(crate) fn write_generated_image_outputs(
    path: &Path,
    output: &[ImageGenerationOutput],
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let mut image_index = 0usize;
    for item in output {
        match item {
            ImageGenerationOutput::Text(text) => {
                if !text.trim().is_empty() {
                    lines.push(text.to_string());
                }
            }
            ImageGenerationOutput::Image {
                mime_type,
                data_base64,
            } => {
                image_index += 1;
                let bytes = base64::engine::general_purpose::STANDARD.decode(data_base64)?;
                let target = generated_image_output_path(path, image_index, mime_type);
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&target, bytes)?;
                lines.push(format!("image: {}", target.display()));
            }
        }
    }
    if image_index == 0 {
        return Err(anyhow!("image provider returned no image output"));
    }
    Ok(lines)
}

pub(crate) fn generated_image_output_path(
    path: &Path,
    image_index: usize,
    mime_type: &str,
) -> PathBuf {
    if image_index == 1 {
        return path.to_path_buf();
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(ToString::to_string)
        .or_else(|| image_extension_for_mime_type(mime_type).map(ToString::to_string))
        .unwrap_or_else(|| "img".to_string());
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("image");
    path.with_file_name(format!("{stem}-{image_index}.{extension}"))
}

pub(crate) fn image_extension_for_mime_type(mime_type: &str) -> Option<&'static str> {
    match mime_type {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        _ => None,
    }
}

pub(crate) async fn try_run_package_command() -> Result<bool> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(command) = args.first().map(String::as_str) else {
        return Ok(false);
    };
    match command {
        "install" => {
            run_package_install(&args[1..])?;
            Ok(true)
        }
        "remove" | "uninstall" => {
            run_package_remove(&args[1..])?;
            Ok(true)
        }
        "update" => {
            run_package_update(&args[1..])?;
            Ok(true)
        }
        "list" => {
            run_package_list(&args[1..])?;
            Ok(true)
        }
        "config" => {
            run_package_config(&args[1..])?;
            Ok(true)
        }
        "login" => {
            run_auth_login(&args[1..]).await?;
            Ok(true)
        }
        "logout" => {
            run_auth_logout(&args[1..])?;
            Ok(true)
        }
        "auth" => {
            let status = auth_command::run(&args[1..]).await;
            if status != 0 {
                std::process::exit(status);
            }
            Ok(true)
        }
        // Upstream's MCP server manager; refuse instead of prompting the model.
        "mcp" => Err(anyhow!(
            "noel mcp is not available: this port has no MCP support (see docs/rust-rewrite/non-parity-register.md)"
        )),
        // noel's own: install the bun runtime for TypeScript extension
        // hosts into the agent dir (`bin/bun`). Non-interactive.
        "bun" => {
            let rest = &args[1..];
            match rest.first().map(String::as_str) {
                Some("install") | None => {
                    let assume_yes = rest.iter().any(|arg| arg == "-y" || arg == "--yes");
                    let (paths, _import) = noel_config::ConfigPaths::discover(
                        std::env::current_dir()?,
                        None,
                    )?;
                    if !assume_yes {
                        println!(
                            "this downloads the bun runtime (~50 MB) into {}",
                            paths.agent_dir.join("bin").display()
                        );
                        print!("proceed? [y/N] ");
                        use std::io::Write as _;
                        std::io::stdout().flush()?;
                        let mut answer = String::new();
                        std::io::stdin().read_line(&mut answer)?;
                        if !answer.trim().eq_ignore_ascii_case("y") {
                            return Err(anyhow!("declined: bun was not installed"));
                        }
                    }
                    bun_install::install_bun(&paths.agent_dir, |_| Ok(true), true).await?;
                    Ok(true)
                }
                Some(other) => Err(anyhow!("noel bun: unknown subcommand {other} (expected install)")),
            }
        }
        _ => Ok(false),
    }
}
