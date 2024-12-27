#[cfg(feature = "mkl")]
extern crate intel_mkl_src;

#[cfg(feature = "accelerate")]
extern crate accelerate_src;

use clap::{Parser, ValueEnum};
use tracing_subscriber::fmt::time::FormatTime;
use std::io::Write;
use tokenizers::{Tokenizer, AddedToken};

use candle::quantized::{ggml_file, gguf_file};
use candle::Tensor;
use candle_transformers::generation::{LogitsProcessor, Sampling};

use candle_examples::token_output_stream::TokenOutputStream;
use candle_transformers::models::quantized_llama_bitnet as model;
use model::ModelWeights;

const DEFAULT_PROMPT: &str = "My favorite theorem is ";

#[derive(Debug)]
enum Prompt {
    Interactive,
    Chat,
    One(String),
}

#[derive(Clone, Debug, Copy, PartialEq, Eq, ValueEnum)]
enum Which {
    #[value(name = "falcon3-1b-1.58")]
    Falcon3_1b1_58,
    #[value(name = "falcon3-3b-1.58")]
    Falcon3_3b1_58,
    #[value(name = "falcon3-7b-1.58")]
    Falcon3_7b1_58,
    #[value(name = "falcon3-10b-1.58")]
    Falcon3_10b1_58,
    #[value(name = "llama3-8b-1.58")]
    Llama3_8b1_58,
}

impl Which {
    fn is_falcon(&self) -> bool {
        matches!(self, Self::Falcon3_1b1_58 | Self::Falcon3_3b1_58 | Self::Falcon3_7b1_58 | Self::Falcon3_10b1_58)
    }

    fn is_llama(&self) -> bool {
        matches!(self, Self::Llama3_8b1_58)
    }

    fn tokenizer_repo(&self) -> &'static str {
        match self {
            Self::Falcon3_1b1_58 => "tiiuae/Falcon3-1B-Instruct-1.58bit",
            Self::Falcon3_3b1_58 => "tiiuae/Falcon3-3B-Instruct-1.58bit",
            Self::Llama3_8b1_58 => "HF1BitLLM/Llama3-8B-1.58-100B-tokens",
            Self::Falcon3_10b1_58 => "tiiuae/Falcon3-10B-Instruct-1.58bit",
            Self::Falcon3_7b1_58 => "tiiuae/Falcon3-7B-Instruct-1.58bit",
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// GGML/GGUF file to load, typically a .bin/.gguf file generated by the quantize command from l
    /// lama.cpp
    #[arg(long)]
    model: Option<String>,

    /// The initial prompt, use 'interactive' for entering multiple prompts in an interactive way
    /// and 'chat' for an interactive model where history of previous prompts and generated tokens
    /// is preserved.
    #[arg(long)]
    prompt: Option<String>,

    /// The length of the sample to generate (in tokens).
    #[arg(short = 'n', long, default_value_t = 1000)]
    sample_len: usize,

    /// The tokenizer config in json format.
    #[arg(long)]
    tokenizer: Option<String>,

    /// The temperature used to generate samples, use 0 for greedy sampling.
    #[arg(long, default_value_t = 0.2)]
    temperature: f64,

    /// Nucleus sampling probability cutoff.
    #[arg(long)]
    top_p: Option<f64>,

    /// Only sample among the top K samples.
    #[arg(long)]
    top_k: Option<usize>,

    /// The seed to use when generating random samples.
    #[arg(long, default_value_t = 299792458)]
    seed: u64,

    /// Enable tracing (generates a trace-timestamp.json file).
    #[arg(long)]
    tracing: bool,

    /// Display the token for the specified prompt.
    #[arg(long)]
    verbose_prompt: bool,

    /// Process prompt elements separately.
    #[arg(long)]
    split_prompt: bool,

    /// Run on CPU rather than GPU even if a GPU is available.
    #[arg(long)]
    cpu: bool,

    /// Penalty to be applied for repeating tokens, 1. means no penalty.
    #[arg(long, default_value_t = 1.5)]
    repeat_penalty: f32,

    /// The context size to consider for the repeat penalty.
    #[arg(long, default_value_t = 64)]
    repeat_last_n: usize,

    /// The model size to use.
    #[arg(long, default_value = "falcon3-1b-1.58")]
    which: Which,

    /// Group-Query Attention, use 8 for the 70B version of LLaMAv2.
    #[arg(long)]
    gqa: Option<usize>,

    /// Use the slower dmmv cuda kernel.
    #[arg(long)]
    force_dmmv: bool,
}

impl Args {
    fn tokenizer(&self) -> anyhow::Result<Tokenizer> {
        let tokenizer_path = match &self.tokenizer {
            Some(config) => std::path::PathBuf::from(config),
            None => {
                let api = hf_hub::api::sync::Api::new()?;
                let repo = self.which.tokenizer_repo();
                let api = api.model(repo.to_string());
                api.get("tokenizer.json")?
            }
        };
        Tokenizer::from_file(tokenizer_path).map_err(anyhow::Error::msg)
    }

    fn model(&self) -> anyhow::Result<std::path::PathBuf> {
        let model_path = match &self.model {
            Some(config) => std::path::PathBuf::from(config),
            None => {
                let (repo, filename) = match self.which {
                    Which::Falcon3_1b1_58 => (
                        "tiiuae/Falcon3-1B-Instruct-1.58bit",
                        "Falcon3-1B-Instruct-1.58bit.gguf",
                    ),
                    Which::Falcon3_3b1_58 => (
                        "tiiuae/Falcon3-3B-Instruct-1.58bit",
                        "Falcon3-3B-Instruct-1.58bit.gguf",
                    ),
                    Which::Falcon3_10b1_58 => (
                        "tiiuae/Falcon3-10B-Instruct-1.58bit",
                        "Falcon3-10B-Instruct-1.58bit.gguf",
                    ),
                    Which::Falcon3_7b1_58 => (
                        "tiiuae/Falcon3-7B-Instruct-1.58bit",
                        "Falcon3-7B-Instruct-1.58bit.gguf",
                    ),
                    Which::Llama3_8b1_58 => (
                        "HF1BitLLM/Llama3-8B-1.58-100B-tokens",
                        "Llama3-8B-1.58bit.gguf",
                    ),
                };
                let revision = "main";
                let api = hf_hub::api::sync::Api::new()?;
                api.repo(hf_hub::Repo::with_revision(
                    repo.to_string(),
                    hf_hub::RepoType::Model,
                    revision.to_string(),
                ))
                .get(filename)?
            }
        };
        Ok(model_path)
    }
}

fn format_size(size_in_bytes: usize) -> String {
    if size_in_bytes < 1_000 {
        format!("{}B", size_in_bytes)
    } else if size_in_bytes < 1_000_000 {
        format!("{:.2}KB", size_in_bytes as f64 / 1e3)
    } else if size_in_bytes < 1_000_000_000 {
        format!("{:.2}MB", size_in_bytes as f64 / 1e6)
    } else {
        format!("{:.2}GB", size_in_bytes as f64 / 1e9)
    }
}

fn main() -> anyhow::Result<()> {
    use tracing_chrome::ChromeLayerBuilder;
    use tracing_subscriber::prelude::*;

    let args = Args::parse();

    #[cfg(feature = "cuda")]
    candle::quantized::cuda::set_force_dmmv(args.force_dmmv);

    candle::cuda::set_gemm_reduced_precision_f16(true);
    candle::cuda::set_gemm_reduced_precision_bf16(true);

    let _guard = if args.tracing {
        let (chrome_layer, guard) = ChromeLayerBuilder::new().build();
        tracing_subscriber::registry().with(chrome_layer).init();
        Some(guard)
    } else {
        None
    };

    println!(
        "avx: {}, neon: {}, simd128: {}, f16c: {}",
        candle::utils::with_avx(),
        candle::utils::with_neon(),
        candle::utils::with_simd128(),
        candle::utils::with_f16c()
    );
    println!(
        "temp: {:.2} repeat-penalty: {:.2} repeat-last-n: {}",
        args.temperature, args.repeat_penalty, args.repeat_last_n
    );

    let model_path = args.model()?;
    let mut file = std::fs::File::open(&model_path)?;
    let start = std::time::Instant::now();
    let device = candle_examples::device(args.cpu)?;

    let mut model = match model_path.extension().and_then(|v| v.to_str()) {
        Some("gguf") => {
            let model = gguf_file::Content::read(&mut file).map_err(|e| e.with_path(model_path))?;
            let mut total_size_in_bytes = 0;
            for (_, tensor) in model.tensor_infos.iter() {
                let elem_count = tensor.shape.elem_count();
                total_size_in_bytes +=
                    elem_count * tensor.ggml_dtype.type_size() / tensor.ggml_dtype.block_size();
            }
            println!(
                "loaded {:?} tensors ({}) in {:.2}s",
                model.tensor_infos.len(),
                &format_size(total_size_in_bytes),
                start.elapsed().as_secs_f32(),
            );
            ModelWeights::from_gguf(model, &mut file, &device)?
        }
        Some("ggml" | "bin") | Some(_) | None => {
            let model = ggml_file::Content::read(&mut file, &device)
                .map_err(|e| e.with_path(model_path))?;
            let mut total_size_in_bytes = 0;
            for (_, tensor) in model.tensors.iter() {
                let elem_count = tensor.shape().elem_count();
                total_size_in_bytes +=
                    elem_count * tensor.dtype().type_size() / tensor.dtype().block_size();
            }
            println!(
                "loaded {:?} tensors ({}) in {:.2}s",
                model.tensors.len(),
                &format_size(total_size_in_bytes),
                start.elapsed().as_secs_f32(),
            );
            println!("params: {:?}", model.hparams);
            let default_gqa = 0;
            ModelWeights::from_ggml(model, args.gqa.unwrap_or(default_gqa))?
        }
    };
    println!("model built");

    let tokenizer = args.tokenizer()?;

    let mut tos = TokenOutputStream::new(tokenizer);
    let prompt = match args.prompt.as_deref() {
        Some("chat") => Prompt::Chat,
        Some("interactive") => Prompt::Interactive,
        Some(s) => Prompt::One(s.to_string()),
        None => Prompt::One(DEFAULT_PROMPT.to_string()),
    };

    let mut pre_prompt_tokens = vec![];
    for prompt_index in 0.. {
        let prompt_str = match &prompt {
            Prompt::One(prompt) => {
                if args.which.is_falcon() {
                    format!("<|user|>\n{prompt}\n<|assistant|>")
                } else if args.which.is_llama() {
                    format!(
                        "{prompt}"
                    )
                } else {
                    prompt.clone()
                }
            }
            Prompt::Interactive | Prompt::Chat => {
                let is_interactive = matches!(prompt, Prompt::Interactive);
                print!("> ");
                std::io::stdout().flush()?;
                let mut prompt = String::new();
                std::io::stdin().read_line(&mut prompt)?;
                if prompt.ends_with('\n') {
                    prompt.pop();
                    if prompt.ends_with('\r') {
                        prompt.pop();
                    }
                }
                if args.which.is_falcon() {
                    format!("<|user|>\n{prompt}\n<|assistant|>")
                } else if args.which.is_llama() {
                    format!(
                        "<|start_header_id|>user<|end_header_id|>\n\n{prompt}\n<|eot_id|><|start_header_id|>assistant<|end_header_id|>"
                    )
                } else {
                    prompt
                }
            }
        };
        print!("{}", &prompt_str);
        let tokens = tos
            .tokenizer()
            .encode(prompt_str, true)
            .map_err(anyhow::Error::msg)?;
        if args.verbose_prompt {
            for (token, id) in tokens.get_tokens().iter().zip(tokens.get_ids().iter()) {
                let token = token.replace('▁', " ").replace("<0x0A>", "\n");
                println!("{id:7} -> '{token}'");
            }
        }

        let prompt_tokens = [&pre_prompt_tokens, tokens.get_ids()].concat();
        let to_sample = args.sample_len.saturating_sub(1);
        let prompt_tokens = if prompt_tokens.len() + to_sample > model::MAX_SEQ_LEN - 10 {
            let to_remove = prompt_tokens.len() + to_sample + 10 - model::MAX_SEQ_LEN;
            prompt_tokens[prompt_tokens.len().saturating_sub(to_remove)..].to_vec()
        } else {
            prompt_tokens
        };
        let mut all_tokens = vec![];
        let mut logits_processor = {
            let temperature = args.temperature;
            let sampling = if temperature <= 0. {
                Sampling::ArgMax
            } else {
                match (args.top_k, args.top_p) {
                    (None, None) => Sampling::All { temperature },
                    (Some(k), None) => Sampling::TopK { k, temperature },
                    (None, Some(p)) => Sampling::TopP { p, temperature },
                    (Some(k), Some(p)) => Sampling::TopKThenTopP { k, p, temperature },
                }
            };
            LogitsProcessor::from_sampling(args.seed, sampling)
        };

        let start_prompt_processing = std::time::Instant::now();
        let mut next_token = if !args.split_prompt {
            let input = Tensor::new(prompt_tokens.as_slice(), &device)?.unsqueeze(0)?;
            let logits = model.forward(&input, 0)?;
            let logits = logits.squeeze(0)?;
            logits_processor.sample(&logits)?
        } else {
            let mut next_token = 0;
            for (pos, token) in prompt_tokens.iter().enumerate() {
                let input = Tensor::new(&[*token], &device)?.unsqueeze(0)?;
                let logits = model.forward(&input, pos)?;
                let logits = logits.squeeze(0)?;
                next_token = logits_processor.sample(&logits)?
            }
            next_token
        };
        let prompt_dt = start_prompt_processing.elapsed();
        all_tokens.push(next_token);
        if let Some(t) = tos.next_token(next_token)? {
            print!("{t}");
            std::io::stdout().flush()?;
        }

        let eos_token = match args.which {
            Which::Falcon3_10b1_58 | Which::Falcon3_7b1_58 | Which::Falcon3_3b1_58 | Which::Falcon3_1b1_58 => "<|endoftext|>",
            Which::Llama3_8b1_58 => "<|eot_id|>",
        };
        
        let eos_token = *tos.tokenizer().get_vocab(true).get(eos_token).unwrap();

        let start_post_prompt = std::time::Instant::now();
        let mut sampled = 0;
        for index in 0..to_sample {
            let input = Tensor::new(&[next_token], &device)?.unsqueeze(0)?;
            let logits = model.forward(&input, prompt_tokens.len() + index)?;
            let logits = logits.squeeze(0)?;
            let logits = if args.repeat_penalty == 1. {
                logits
            } else {
                let start_at = all_tokens.len().saturating_sub(args.repeat_last_n);
                candle_transformers::utils::apply_repeat_penalty(
                    &logits,
                    args.repeat_penalty,
                    &all_tokens[start_at..],
                )?
            };
            next_token = logits_processor.sample(&logits)?;
            all_tokens.push(next_token);
            if let Some(t) = tos.next_token(next_token)? {
                print!("{t}");
                std::io::stdout().flush()?;
            }
            sampled += 1;
            if next_token == eos_token {
                break;
            };
        }
        if let Some(rest) = tos.decode_rest().map_err(candle::Error::msg)? {
            print!("{rest}");
        }
        std::io::stdout().flush()?;
        let dt = start_post_prompt.elapsed();
        println!(
            "\n\n{:4} prompt tokens processed: {:.2} token/s",
            prompt_tokens.len(),
            prompt_tokens.len() as f64 / prompt_dt.as_secs_f64(),
        );
        println!(
            "{sampled:4} tokens generated: {:.2} token/s",
            sampled as f64 / dt.as_secs_f64(),
        );

        match prompt {
            Prompt::One(_) => break,
            Prompt::Interactive => {}
            Prompt::Chat => {
                pre_prompt_tokens = [prompt_tokens.as_slice(), all_tokens.as_slice()].concat()
            }
        }
    }

    Ok(())
}
