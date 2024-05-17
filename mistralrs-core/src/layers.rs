#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::{
    collections::HashMap,
    ops::Mul,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use candle_core::{
    quantized::{gguf_file, QMatMul, QTensor},
    DType, Device, IndexOp, Result, Tensor, WithDType,
};
use candle_nn::{
    layer_norm::{RmsNormNonQuantized, RmsNormQuantized},
    Linear, Module, VarBuilder,
};
use once_cell::sync::Lazy;

static MASKS: Lazy<Mutex<HashMap<(usize, usize), Tensor>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

use crate::{cublaslt::CUBLASLT_HANDLE, models::phi3, INHIBIT_GEMM_F16};

#[derive(Debug, Clone)]
pub struct RmsNorm {
    inner: candle_nn::RmsNorm<RmsNormNonQuantized>,
    eps: f64,
    weight: Tensor,
}

impl RmsNorm {
    pub fn new(size: usize, eps: f64, vb: VarBuilder) -> Result<Self> {
        let inner = candle_nn::rms_norm_non_quant(size, eps, vb)?;
        let w = inner.inner().weight().clone();
        Ok(Self {
            inner,
            eps,
            weight: w,
        })
    }

    pub fn from_w(w: Tensor, eps: f64) -> Result<Self> {
        let inner = candle_nn::RmsNorm::<RmsNormNonQuantized>::new(w.clone(), eps);
        Ok(Self {
            inner,
            eps,
            weight: w,
        })
    }
}

impl Module for RmsNorm {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        if x.device().is_cpu() {
            // Handle device mapping case
            return candle_nn::ops::rms_norm(&x.contiguous()?, &self.weight, self.eps as f32);
        }
        self.inner.forward(x)
    }
}

#[derive(Debug, Clone)]
pub struct QRmsNorm {
    inner: candle_nn::RmsNorm<RmsNormQuantized>,
}

impl QRmsNorm {
    pub fn new(scale: QTensor, eps: f32) -> Result<Self> {
        let scale = scale.dequantize(&scale.device())?;
        let inner = candle_nn::RmsNorm::<RmsNormQuantized>::new(scale, eps as f64);
        Ok(Self { inner })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        self.inner.forward(x)
    }
}

/// RoPE supporting LongRope
#[derive(Debug, Clone)]
pub struct PhiRotaryEmbedding {
    short_sin: Tensor,
    short_cos: Tensor,
    long_cos: Option<Tensor>,
    long_sin: Option<Tensor>,
    original_max_position_embeddings: usize,
}

#[derive(Debug, Clone)]
enum ScaledRopeType {
    Su,
    Yarn,
}

impl FromStr for ScaledRopeType {
    type Err = candle_core::Error;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "su" => Ok(Self::Su),
            "yarn" => Ok(Self::Yarn),
            _ => Err(candle_core::Error::Msg(
                "Expected either `su` or `yarn` scaled RoPE type.".to_string(),
            )),
        }
    }
}

#[derive(Debug, Clone)]
struct ScaledRopeParams {
    short_factor: Vec<f32>,
    long_factor: Vec<f32>,
    scaling_type: ScaledRopeType,
}

impl PhiRotaryEmbedding {
    pub fn new(dtype: DType, cfg: &phi3::Config, dev: &Device) -> Result<Self> {
        let scaled_params = cfg.rope_scaling.as_ref().map(|r| ScaledRopeParams {
            short_factor: r["short_factor"].clone().left().unwrap(),
            long_factor: r["long_factor"].clone().left().unwrap(),
            scaling_type: r["type"].clone().right().unwrap().parse().unwrap(),
        });
        let max_seq_len = cfg.max_position_embeddings;
        let dim = cfg.head_dim();

        if let Some(scaled_params) = scaled_params {
            // Calculate scale
            let scale =
                cfg.max_position_embeddings as f64 / cfg.original_max_position_embeddings as f64;
            let scaling_factor = if scale <= 1.0 {
                1.0
            } else {
                match scaled_params.scaling_type {
                    ScaledRopeType::Su => (1.0
                        + scale.ln() / (cfg.original_max_position_embeddings as f64).ln())
                    .sqrt(),
                    ScaledRopeType::Yarn => 0.1 * scale.ln() + 1.0,
                }
            };

            // Calculate inv freqs for short, long
            let inv_freq_long: Vec<_> = (0..dim)
                .step_by(2)
                .enumerate()
                .map(|(k, i)| {
                    1f32 / (scaled_params.long_factor[k]
                        * cfg.rope_theta.powf(i as f64 / dim as f64) as f32)
                })
                .collect();
            let inv_freq_short: Vec<_> = (0..dim)
                .step_by(2)
                .enumerate()
                .map(|(k, i)| {
                    1f32 / (scaled_params.short_factor[k]
                        * cfg.rope_theta.powf(i as f64 / dim as f64) as f32)
                })
                .collect();
            let inv_freq_len = inv_freq_long.len();

            let t = Tensor::arange(0u32, max_seq_len as u32, dev)?
                .to_dtype(DType::F32)?
                .reshape((max_seq_len, 1))?;

            // Calculate sin,cos for long
            let inv_freq_long = Tensor::from_vec(inv_freq_long, (1, inv_freq_len), dev)?;
            let freqs_long = t.matmul(&inv_freq_long)?;
            let long_sin = freqs_long.sin()?.mul(scaling_factor)?.to_dtype(dtype)?;
            let long_cos = freqs_long.cos()?.mul(scaling_factor)?.to_dtype(dtype)?;

            // Calculate sin,cos for short
            let inv_freq_short = Tensor::from_vec(inv_freq_short, (1, inv_freq_len), dev)?;
            let freqs_short = t.matmul(&inv_freq_short)?;
            let short_sin = freqs_short.sin()?.mul(scaling_factor)?.to_dtype(dtype)?;
            let short_cos = freqs_short.cos()?.mul(scaling_factor)?.to_dtype(dtype)?;

            Ok(Self {
                short_cos,
                short_sin,
                long_cos: Some(long_cos),
                long_sin: Some(long_sin),
                original_max_position_embeddings: cfg.original_max_position_embeddings,
            })
        } else {
            let inv_freq: Vec<_> = (0..dim)
                .step_by(2)
                .map(|i| 1f32 / cfg.rope_theta.powf(i as f64 / dim as f64) as f32)
                .collect();
            let inv_freq_len = inv_freq.len();
            let inv_freq = Tensor::from_vec(inv_freq, (1, inv_freq_len), dev)?;
            let t = Tensor::arange(0u32, max_seq_len as u32, dev)?
                .to_dtype(DType::F32)?
                .reshape((max_seq_len, 1))?;
            let freqs = t.matmul(&inv_freq)?;
            let sin = freqs.sin()?.to_dtype(dtype)?;
            let cos = freqs.cos()?.to_dtype(dtype)?;
            Ok(Self {
                short_cos: cos,
                short_sin: sin,
                long_cos: None,
                long_sin: None,
                original_max_position_embeddings: cfg.original_max_position_embeddings,
            })
        }
    }

    /// Returns (sin, cos) taking into account LongRope
    fn get_long_or_short_sin_cos(&self, position_ids: &[usize]) -> (&Tensor, &Tensor) {
        if self.long_cos.is_none() {
            return (&self.short_sin, &self.short_cos);
        }
        let seq_len = position_ids.iter().max().unwrap() + 1;
        if seq_len > self.original_max_position_embeddings {
            (
                self.long_sin.as_ref().unwrap(),
                self.long_cos.as_ref().unwrap(),
            )
        } else {
            (&self.short_sin, &self.short_cos)
        }
    }

    pub fn forward(
        &self,
        q: &Tensor,
        k: &Tensor,
        seqlen_offsets: &[usize],
        position_ids: &[usize],
    ) -> Result<(Tensor, Tensor)> {
        let (_b_sz, _h, seq_len, _n_embd) = q.dims4()?;
        let mut q_embeds = Vec::new();
        let mut k_embeds = Vec::new();
        let (sin, cos) = self.get_long_or_short_sin_cos(position_ids);
        for (i, offset) in seqlen_offsets.iter().enumerate() {
            let cos = cos.narrow(0, *offset, seq_len)?;
            let sin = sin.narrow(0, *offset, seq_len)?;
            let q_embed =
                candle_nn::rotary_emb::rope(&q.i(i)?.unsqueeze(0)?.contiguous()?, &cos, &sin)?;
            let k_embed =
                candle_nn::rotary_emb::rope(&k.i(i)?.unsqueeze(0)?.contiguous()?, &cos, &sin)?;
            q_embeds.push(q_embed);
            k_embeds.push(k_embed);
        }
        Ok((Tensor::cat(&q_embeds, 0)?, Tensor::cat(&k_embeds, 0)?))
    }
}

// https://github.com/huggingface/transformers/blob/main/src/transformers/modeling_attn_mask_utils.py
pub struct CausalMasker;

// https://github.com/mokeyish/candle-ext/blob/main/src/triangular.rs
fn apply_tril(xs: &Tensor, diagonal: isize) -> Result<Tensor> {
    let device = xs.device();
    let (l, s) = xs.dims2()?;
    let mut xs_tri = vec![];
    for i in 0..l as isize {
        for j in 0..s as isize {
            let cond = i + diagonal < j;
            xs_tri.push(if cond { 0u8 } else { 1u8 });
        }
    }
    xs * Tensor::from_vec(xs_tri, (l, s), device)?.to_dtype(xs.dtype())?
}

// https://github.com/mokeyish/candle-ext/blob/main/src/masked_fill.rs
/// xs are on false (0), value is on true (1)
fn masked_fill<D: WithDType>(xs: &Tensor, mask: &Tensor, value: D) -> Result<Tensor> {
    let on_true = Tensor::full(value, xs.shape(), xs.device())?;
    let on_false = xs;
    let res = mask
        .broadcast_as(xs.shape())?
        .where_cond(&on_true, on_false)?;
    Ok(res)
}

impl CausalMasker {
    fn make_mask(&self, tgt_len: usize, past_kv_len: usize, device: &Device) -> Result<Tensor> {
        let offset = tgt_len + past_kv_len;
        let mask: Vec<_> = (0..tgt_len)
            .flat_map(|i| (0..offset).map(move |j| u8::from(j + tgt_len > i + offset)))
            .collect();
        Tensor::from_slice(&mask, (tgt_len, offset), device)
    }

    pub fn calculate_past_kv_len(
        &self,
        cache: &[Option<(Tensor, Tensor)>],
    ) -> candle_core::Result<usize> {
        let kv_cache_1 = &cache[0];
        if kv_cache_1.is_none() {
            return Ok(0);
        }
        let k_cache_1 = &kv_cache_1.as_ref().unwrap().0;
        return Ok(k_cache_1.dims()[2]);
    }

    pub fn make_causal_mask_as_attn_bias(
        &self,
        input_ids: &Tensor,
        cache: &[Option<(Tensor, Tensor)>],
        dtype: DType,
        n_head: usize,
    ) -> Result<Option<Tensor>> {
        let zero = Tensor::new(0.0f32, input_ids.device())?;
        let causal_mask = self.make_causal_mask(input_ids, cache)?;
        Ok(causal_mask.map(|mask| {
            let mask = mask
                .broadcast_as((mask.dims()[0], n_head, mask.dims()[2], mask.dims()[3]))
                .expect("Failed to pre broadcast mask");
            // Mask: 1 means use from x (add 0.0), 0 means mask out (add -inf)
            masked_fill(
                &zero
                    .to_dtype(dtype)
                    .expect("Failed to create mask")
                    .broadcast_as(mask.shape())
                    .expect("Failed to create mask"),
                &mask,
                f32::NEG_INFINITY,
            )
            .expect("Failed to create mask")
        }))
    }

    pub fn make_causal_mask(
        &self,
        input_ids: &Tensor,
        cache: &[Option<(Tensor, Tensor)>],
    ) -> Result<Option<Tensor>> {
        let past_kv_len = self.calculate_past_kv_len(cache)?;
        let (b_sz, tgt_len) = input_ids.dims2()?;
        if tgt_len == 1 {
            return Ok(None);
        }
        let res = MASKS.lock().unwrap().get(&(tgt_len, past_kv_len)).cloned();
        if let Some(mask) = res {
            Ok(Some(mask))
        } else {
            let mask = self.make_mask(tgt_len, past_kv_len, input_ids.device())?;
            let mask = mask
                .expand((b_sz, 1, tgt_len, tgt_len + past_kv_len))?
                .to_dtype(DType::U8)?;

            MASKS
                .lock()
                .unwrap()
                .insert((tgt_len, past_kv_len), mask.clone());
            Ok(Some(mask))
        }
    }

    pub fn make_causal_mask_with_sliding_window(
        &self,
        input_ids: &Tensor,
        cache: &[Option<(Tensor, Tensor)>],
        sliding_window: Option<usize>,
    ) -> Result<Option<Tensor>> {
        if sliding_window.is_none() {
            return self.make_causal_mask(input_ids, cache);
        }
        let sliding_window = sliding_window.unwrap();
        let past_kv_len = self.calculate_past_kv_len(cache)?;
        let (b_sz, tgt_len) = input_ids.dims2()?;
        if tgt_len == 1 {
            return Ok(None);
        }
        let res = MASKS.lock().unwrap().get(&(tgt_len, past_kv_len)).cloned();
        if let Some(mask) = res {
            Ok(Some(mask))
        } else {
            let mask = self.make_mask(tgt_len, past_kv_len, input_ids.device())?;
            let diagonal = past_kv_len as isize - sliding_window as isize - 1;
            let context_mask = apply_tril(&mask.ones_like()?, diagonal)?;
            let mask = masked_fill(&mask.to_dtype(DType::F32)?, &context_mask, f32::MIN)?;
            let mask = mask
                .expand((b_sz, 1, tgt_len, tgt_len + past_kv_len))?
                .to_dtype(DType::U8)?;

            MASKS
                .lock()
                .unwrap()
                .insert((tgt_len, past_kv_len), mask.clone());
            Ok(Some(mask))
        }
    }

    pub fn apply_mask(
        &self,
        mask: &Option<Tensor>,
        att: Tensor,
        neg_inf: &Tensor,
    ) -> Result<Tensor> {
        match mask {
            None => Ok(att),
            Some(mask) => {
                let mask = mask.broadcast_as(att.shape())?;
                mask.where_cond(
                    &neg_inf
                        .to_device(att.device())?
                        .to_dtype(att.dtype())?
                        .broadcast_as(att.dims())?,
                    &att,
                )
            }
        }
    }
}

/// Matrix multiplcation, configurable to be via f16 (to use the faster GEMM kernels) optionally.
pub struct MatMul;

/// Set the matmuls to go via f16
pub(crate) static USE_MATMUL_VIA_F16: AtomicBool = AtomicBool::new(false);

pub(crate) fn set_use_matmul_via_f16(via_f16: bool) {
    if !INHIBIT_GEMM_F16.load(Ordering::Relaxed) {
        USE_MATMUL_VIA_F16.store(via_f16, Ordering::Relaxed)
    }
}
pub fn get_use_matmul_via_f16() -> bool {
    USE_MATMUL_VIA_F16.load(Ordering::Relaxed)
}

impl MatMul {
    /// Compute matrix-matrix product, optionally casting to f16 to use specialized GEMM kernels.
    pub fn matmul(&self, a: &Tensor, b: &Tensor) -> Result<Tensor> {
        if !get_use_matmul_via_f16() {
            return a.matmul(b);
        }
        let original_dtype = a.dtype();
        a.to_dtype(DType::F16)?
            .matmul(&b.to_dtype(DType::F16)?)?
            .to_dtype(original_dtype)
    }

    /// Compute matrix-matrix product, optionally casting to f16 to use specialized GEMM kernels.
    /// The result will be divided by the `scale` parameter in an affine division.
    pub fn matmul_affine_div(&self, a: &Tensor, b: &Tensor, scale: f64) -> Result<Tensor> {
        // TODO(EricLBuehler): Optimize this by using the gemm parameter
        self.matmul(a, b)? / scale
    }

    /// Compute quantized matrix-matrix product, optionally casting to f16 to use specialized GEMM kernels.
    pub fn qmatmul(&self, x: &Tensor, matmul: &QMatMul) -> Result<Tensor> {
        if get_use_matmul_via_f16() {
            matmul.forward_via_f16(x)
        } else {
            matmul.forward(x)
        }
    }
}

pub struct ScaledDotProductAttention;

impl ScaledDotProductAttention {
    /// Computes softmax(QK^T*sqrt(d_k))V
    /// The attention implemention is dispatched as follows:
    /// 1) If `use_flash_attn == true`, use a flash attention V2 kernel
    /// 2) If using CUDA and the cuBLASLt kernel is initialized, then it will use an optimized version.
    /// 3) Otherwise, use the "naive" SDPA implementation.
    #[allow(unused_variables)]
    pub fn run_attention(
        &self,
        q: &Tensor,
        k: &Tensor,
        v: &Tensor,
        n_attn_heads: usize,
        head_dim: usize,
        mask: Option<&Tensor>,
        use_flash_attn: bool,
        b_sz: usize,
        seq_len: usize,
    ) -> Result<Tensor> {
        if use_flash_attn {
            // flash-attn expects (b_sz, seq_len, nheads, head_dim)
            let q = q.transpose(1, 2)?;
            let k = k.transpose(1, 2)?;
            let v = v.transpose(1, 2)?;
            let softmax_scale = 1f32 / (head_dim as f32).sqrt();
            return flash_attn(&q, &k, &v, softmax_scale, seq_len > 1)?.transpose(1, 2);
        }

        if let (Device::Cuda(_), Some(cublaslt)) = (q.device(), *CUBLASLT_HANDLE.lock().unwrap()) {
            #[cfg(feature = "cuda")]
            {
                // cuBLASLt batch matmul implementation requires inputs to be dims3
                let k = k.flatten(0, 1)?;
                let q = q.flatten(0, 1)?;
                let v = v.flatten(0, 1)?;
                let attention_bias = mask.map(|mask| mask.flatten(0, 1)).transpose()?;

                // If attention_bias is set, we fuse the add by giving it as the output matrix
                // and setting beta to 1.0
                let beta = match attention_bias.is_some() {
                    true => Some(1.0),
                    false => None,
                };

                // Batch matrix multiplication
                // Fuse softmax scale and attention_bias add
                let attention_scores = cublaslt.batch_matmul(
                    &k,
                    &q,
                    attention_bias.as_ref(),
                    Some((1.0 / (head_dim as f64).sqrt()) as f32),
                    beta,
                    None,
                    None,
                )?;
                let attention_probs = candle_nn::ops::softmax_last_dim(&attention_scores)?;

                let context_layer = cublaslt.batch_matmul(
                    &v.t()?.contiguous()?,
                    &attention_probs,
                    // We save one allocation
                    Some(&q),
                    None,
                    None,
                    None,
                    None,
                )?;

                // Reshape to dims4
                context_layer.reshape((b_sz, n_attn_heads, seq_len, head_dim))
            }
            #[cfg(not(feature = "cuda"))]
            {
                candle_core::bail!("`cuda` feature is not enabled")
            }
        } else {
            let att = MatMul.matmul_affine_div(
                &q.contiguous()?,
                &k.t()?.contiguous()?,
                (head_dim as f64).sqrt(),
            )?;

            let att = match mask {
                Some(m) => att.broadcast_add(&m)?,
                None => att,
            };
            let att = candle_nn::ops::softmax_last_dim(&att)?;
            // Convert to contiguous as matmul doesn't support strided vs for now.
            MatMul.matmul(&att, &v.contiguous()?)
        }
    }
}

#[derive(Debug, Clone)]
pub struct QLinear {
    inner: QMatMul,
    bias: Option<Tensor>,
    dtype: DType,
}

impl QLinear {
    pub fn new<R: std::io::Read + std::io::Seek>(
        ct: &gguf_file::Content,
        r: &mut R,
        name: &str,
        device: &Device,
    ) -> Result<Self> {
        let w = ct.tensor(r, &format!("{name}.weight"), device)?;
        let b = ct.tensor(r, &format!("{name}.bias"), device)?;
        let inner = QMatMul::from_qtensor(w)?;
        let bias = b.dequantize(device)?;
        Ok(Self {
            inner,
            bias: Some(bias),
            dtype: DType::F32,
        })
    }

    pub fn from_linear(linear: Linear) -> Self {
        Self {
            inner: QMatMul::Tensor(linear.weight().clone()),
            bias: linear.bias().cloned(),
            dtype: if linear.weight().device().is_cuda() {
                DType::BF16
            } else {
                DType::F32
            },
        }
    }

    pub fn from_parts(w: Tensor, b: Option<Tensor>) -> Self {
        let dtype = if w.device().is_cuda() {
            DType::BF16
        } else {
            DType::F32
        };
        Self {
            inner: QMatMul::Tensor(w),
            bias: b,
            dtype,
        }
    }

    pub fn from_qparts(w: QTensor, b: Option<Tensor>) -> Self {
        if let Some(ref b) = b {
            assert_eq!(b.dtype(), DType::F32);
        }
        Self {
            inner: QMatMul::QTensor(Arc::new(w)),
            bias: b,
            dtype: DType::F32,
        }
    }

    pub fn inner(&mut self) -> &mut QMatMul {
        &mut self.inner
    }

    pub fn is_quant(&self) -> bool {
        matches!(self.inner, QMatMul::QTensor(_))
    }

    pub fn bias(&self) -> Option<&Tensor> {
        self.bias.as_ref()
    }
}

impl Module for QLinear {
    fn forward(&self, xs: &Tensor) -> Result<Tensor> {
        let xs = if self.is_quant() {
            xs.to_dtype(DType::F32)?
        } else {
            xs.clone()
        };
        let forward_fn = if get_use_matmul_via_f16() {
            QMatMul::forward
        } else {
            QMatMul::forward_via_f16
        };
        if let Some(bias) = &self.bias {
            forward_fn(&self.inner, &xs)?
                .broadcast_add(bias)?
                .to_dtype(self.dtype)
        } else {
            forward_fn(&self.inner, &xs)?.to_dtype(self.dtype)
        }
    }
}

#[cfg(feature = "flash-attn")]
pub fn flash_attn(
    q: &Tensor,
    k: &Tensor,
    v: &Tensor,
    softmax_scale: f32,
    causal: bool,
) -> Result<Tensor> {
    candle_flash_attn::flash_attn(q, k, v, softmax_scale, causal)
}

#[cfg(not(feature = "flash-attn"))]
pub fn flash_attn(_: &Tensor, _: &Tensor, _: &Tensor, _: f32, _: bool) -> Result<Tensor> {
    unimplemented!("Compile with '--features flash-attn'")
}

pub fn verify_sanity_gguf(arch: &str, expected_arch: &str) -> Result<()> {
    if arch != expected_arch {
        candle_core::bail!("Expected `{expected_arch}` architecture, got `{arch}`.");
    }
    Ok(())
}

pub fn repeat_kv(x: Tensor, n_rep: usize) -> Result<Tensor> {
    if n_rep == 1 {
        Ok(x)
    } else {
        let (b_sz, n_kv_head, seq_len, head_dim) = x.dims4()?;
        Tensor::cat(&vec![&x; n_rep], 2)?.reshape((b_sz, n_kv_head * n_rep, seq_len, head_dim))
    }
}
