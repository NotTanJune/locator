//! Mixed batches for image inputs: text tokens and vision-encoder rows go into one llama.cpp
//! batch, which runs in one `llama_process` call and is pooled once. A port of DigUp's
//! `LlamaEmbedder.embedMedia` (MIT; see tests/fixtures/digup-reference/NOTICE.md).
//!
//! Why: `mtmd_helper_eval_chunks` decodes chunk by chunk, and each decode pools on its own, so the
//! pooled vector keeps only the last chunk. Every image then gets the same vector.
//!
//! The only `unsafe` here is the calls into llama.cpp's extended batch API (`llama_batch_ext_*`,
//! `llama_process`), which the safe llama-cpp-2 wrapper does not expose.

use anyhow::{ensure, Result};
use llama_cpp_2::context::LlamaContext;
use llama_cpp_sys_2 as sys;

use super::vector::l2_normalize;

/// One piece of a mixed batch, in order.
pub enum Part<'a> {
    /// Text token ids. Each gets its own position.
    Tokens(&'a [i32]),
    /// `count` encoder rows of `width` floats each (image soft tokens). Each row gets its own position.
    Rows {
        data: &'a [f32],
        count: usize,
        width: usize,
    },
}

/// Frees the batch when dropped, including on error paths.
struct BatchGuard(*mut sys::llama_batch_ext);

impl Drop for BatchGuard {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `llama_batch_ext_init` and is freed exactly once.
        unsafe { sys::llama_batch_ext_free(self.0) }
    }
}

/// Runs `parts` as one input (sequence 0) in a single `llama_process` call and returns the first
/// `dims` floats of its pooled embedding, L2-normalised.
///
/// The context must have been created with embeddings on and mean pooling. Its KV cache is not
/// cleared here: the caller clears it before and after.
pub fn embed_mixed(
    ctx: &mut LlamaContext<'_>,
    parts: &[Part<'_>],
    dims: usize,
) -> Result<Vec<f32>> {
    let ctx: *mut sys::llama_context = ctx.raw_context_ptr();
    ensure!(!ctx.is_null(), "null llama context");
    // SAFETY: `ctx` is the live context borrowed mutably for this call. `batch` is freed by the
    // guard on every path. Every pointer handed to llama.cpp points into a slice that outlives the
    // call, or into `batch`.
    unsafe {
        let batch = sys::llama_batch_ext_init(ctx);
        ensure!(!batch.is_null(), "llama_batch_ext_init returned null");
        let _guard = BatchGuard(batch);
        let mut position: i32 = 0;
        for part in parts {
            match *part {
                Part::Tokens(tokens) => {
                    for &token in tokens {
                        let index = sys::llama_batch_ext_add_token(batch, 0, token);
                        place(batch, index, position)?;
                        position += 1;
                    }
                }
                Part::Rows { data, count, width } => {
                    ensure!(
                        data.len() >= count * width,
                        "encoder rows are shorter than {count} x {width}"
                    );
                    for row in 0..count {
                        let embd = sys::llama_embd {
                            data: data.as_ptr().add(row * width),
                            n_rows: 1,
                            n_embd: width,
                        };
                        let index = sys::llama_batch_ext_add_embd(batch, 0, embd);
                        place(batch, index, position)?;
                        position += 1;
                    }
                }
            }
        }
        let status = sys::llama_process(ctx, sys::LLAMA_PROCESS_TYPE_ENCODE, batch);
        ensure!(status == 0, "llama_process failed ({status})");
        let pooled = sys::llama_get_embeddings_seq(ctx, 0);
        ensure!(!pooled.is_null(), "no pooled embedding for the batch");
        let vector = std::slice::from_raw_parts(pooled, dims).to_vec();
        Ok(l2_normalize(&vector))
    }
}

/// Sets the position and the output flag of the entry just added, as DigUp does (every token
/// produces an output, so the pooled vector covers all of them).
///
/// # Safety
/// `batch` must be a live batch and `index` the value its `llama_batch_ext_add_*` call returned.
unsafe fn place(batch: *mut sys::llama_batch_ext, index: i32, position: i32) -> Result<()> {
    ensure!(index >= 0, "batch is full or an entry is invalid ({index})");
    let pos: [i32; 4] = [position, 0, 0, 0];
    ensure!(
        sys::llama_batch_ext_set_pos(batch, index, pos.as_ptr()),
        "cannot set the position of entry {index}"
    );
    ensure!(
        sys::llama_batch_ext_set_output_embd(batch, index, true),
        "cannot mark entry {index} for output"
    );
    Ok(())
}
