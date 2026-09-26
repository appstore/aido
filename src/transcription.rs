//! Silence-aware audio requests and durable, independently committed replies.
use crate::api::GenerateResult;
use crate::domain::{InputContent, InputPart};
use crate::plan::ExecutionPlan;
use crate::processors::RequestStep;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub fn plan_steps(inputs: &[InputPart], seconds: u32) -> Result<Vec<RequestStep>> {
    #[cfg(not(feature = "audio-decode"))]
    {
        let _ = (inputs, seconds);
        bail!("audio splitting requires a build with --features audio-decode");
    }
    #[cfg(feature = "audio-decode")]
    {
        use crate::processors::StepRole;
        let [audio] = inputs else {
            bail!("audio splitting needs exactly one audio input")
        };
        let InputContent::Media(bytes) = &audio.content else {
            bail!("expected audio bytes")
        };
        let buffer = crate::audio::decode_mono(bytes, usize::MAX)?;
        let rate = buffer.spec.sample_rate;
        anyhow::ensure!(rate > 0, "audio sample rate must be positive");
        let mut steps = Vec::new();
        for (start, end) in boundaries(&buffer.samples, rate as usize, seconds as usize) {
            let part = InputPart {
                id: audio.id,
                source: audio.source.clone(),
                name: audio.name.clone(),
                kind: audio.kind,
                unknown_kind: audio.unknown_kind,
                mime: "audio/wav".into(),
                content: InputContent::Media(wav(&buffer.samples[start..end], rate)?),
                unit: audio.unit.clone(),
            };
            steps.push(RequestStep {
                index: steps.len(),
                inputs: vec![part],
                label: format!(
                    "audio {:.1}–{:.1}s",
                    start as f64 / rate as f64,
                    end as f64 / rate as f64
                ),
                hard_cut_end: false,
                part: None,
                artifact_stem: None,
                role: StepRole::Map,
            });
        }
        Ok(steps)
    }
}

/// Look backwards up to five seconds (at most half the segment) for a
/// 200ms quiet window. No overlap: preserve every sample exactly once,
/// and never remove legitimate repeated words from adjacent replies.
#[cfg(feature = "audio-decode")]
fn boundaries(samples: &[f32], rate: usize, seconds: usize) -> Vec<(usize, usize)> {
    let target = rate * seconds;
    let search = (5 * rate).min(target / 2);
    let window = (rate / 5).max(1);
    let mut result = Vec::new();
    let mut start = 0;
    while start < samples.len() {
        let mut end = (start + target).min(samples.len());
        if end < samples.len() {
            let lower = end - search;
            let mut cursor = end;
            while cursor >= lower + window {
                let frame = &samples[cursor - window..cursor];
                if frame.iter().all(|s| s.is_finite())
                    && frame.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / (window as f64)
                        < 0.0001
                {
                    end = cursor - window / 2;
                    break;
                }
                cursor -= window;
            }
        }
        result.push((start, end));
        start = end;
    }
    result
}

#[cfg(feature = "audio-decode")]
fn wav(samples: &[f32], rate: u32) -> Result<Vec<u8>> {
    let len = u32::try_from(
        samples
            .len()
            .checked_mul(2)
            .context("audio segment too large")?,
    )?;
    let mut out = Vec::with_capacity(len as usize + 44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(
        &len.checked_add(36)
            .context("audio segment too large")?
            .to_le_bytes(),
    );
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(
        &rate
            .checked_mul(2)
            .context("invalid sample rate")?
            .to_le_bytes(),
    );
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&((sample.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    Ok(out)
}

/// Keep automatic progress separate from prunable history records. Each
/// audio/request identity has its own directory, so defaults never collide.
pub fn default_state_dir(plan: &ExecutionPlan) -> Result<PathBuf> {
    let mut root = crate::history::history_dir()
        .context("cannot locate transcription state directory")?
        .into_os_string();
    root.push("-transcription");
    let key = Sha256::digest(serde_json::to_vec(&identity(plan))?);
    Ok(PathBuf::from(root).join(format!("{key:x}")))
}

fn identity(plan: &ExecutionPlan) -> serde_json::Value {
    // Version the algorithm as well as the serialized format. Hash source
    // bytes AND the actual planned requests, so changed decoders invalidate it.
    let mut hash = Sha256::new();
    for part in plan
        .inputs
        .iter()
        .chain(plan.steps.iter().flat_map(|s| &s.inputs))
    {
        if let InputContent::Media(bytes) = &part.content {
            hash.update((bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    serde_json::json!({
        "version": 1,
        "audio_sha256": format!("{:x}", hash.finalize()),
        "base_url": plan.resolved.base_url,
        "model": plan.resolved.model,
        "options": plan.resolved.options,
        "temperature": plan.resolved.temperature,
        "max_tokens": plan.resolved.max_tokens,
        "instruction": plan.instruction,
        "requirement": plan.requirement,
        "segments": plan.steps.len(),
    })
}

pub struct Checkpoint {
    dir: PathBuf,
}

impl Checkpoint {
    pub fn open(plan: &ExecutionPlan) -> Result<Option<Self>> {
        let Some(dir) = &plan.transcribe_state else {
            return Ok(None);
        };
        let identity = identity(plan);
        std::fs::create_dir_all(dir).context("cannot create transcription state directory")?;
        let manifest = dir.join("manifest.json");
        commit(&manifest, &serde_json::to_vec_pretty(&identity)?)?;
        let existing: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest)?)?;
        if existing != identity {
            bail!("transcription state does not match the audio, model or parameters; use a different --transcribe-state directory");
        }
        Ok(Some(Self { dir: dir.clone() }))
    }

    pub fn load(&self, index: usize) -> Result<Option<GenerateResult>> {
        let path = self.dir.join(format!("segment-{index:06}.json"));
        match std::fs::read(&path) {
            Ok(bytes) => {
                let text: String = serde_json::from_slice(&bytes)
                    .with_context(|| format!("invalid checkpoint {}", path.display()))?;
                Ok(Some(GenerateResult::complete_with_text(text)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, index: usize, reply: &GenerateResult) -> Result<()> {
        if reply.status == crate::domain::GenerationStatus::Complete {
            commit(
                &self.dir.join(format!("segment-{index:06}.json")),
                &serde_json::to_vec(&reply.text)?,
            )?;
        }
        Ok(())
    }
}

/// Publish only fully written files, without replacing another run's commit.
/// No persistent lock: interrupted processes cannot prevent resumption.
fn commit(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path.parent().context("missing checkpoint directory")?;
    let temp = parent.join(format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        match std::fs::hard_link(&temp, path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        #[cfg(unix)]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temp);
    result.with_context(|| format!("cannot save transcription checkpoint {}", path.display()))
}

#[cfg(test)]
mod tests;
