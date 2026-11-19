//! Concurrent CPU Run adapter. No session configuration is changed after loading.
use anyhow::{Result, ensure};
use ort::{AsPointer, session::Session, value::Tensor};

struct Output(*mut ort::sys::OrtValue);
impl Drop for Output {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: an owned, per-call value initialized by OrtApi::Run.
            unsafe { (ort::api().ReleaseValue)(self.0) };
        }
    }
}
struct Shape(*mut ort::sys::OrtTensorTypeAndShapeInfo);
impl Drop for Shape {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned metadata initialized by GetTensorTypeAndShape.
            unsafe { (ort::api().ReleaseTensorTypeAndShapeInfo)(self.0) };
        }
    }
}
fn checked(status: ort::sys::OrtStatusPtr) -> Result<()> {
    // SAFETY: all callers pass an owned status from the public ORT API.
    unsafe { ort::Error::result_from_status(status) }?;
    Ok(())
}
pub(super) fn score(session: &Session, ids: Vec<i64>, mask: Vec<i64>) -> Result<f64> {
    let n = ids.len();
    ensure!(n == mask.len(), "student token/mask length mismatch");
    let ids = Tensor::from_array(([1, n], ids))?;
    let mask = Tensor::from_array(([1, n], mask))?;
    let inputs = [ids.ptr(), mask.ptr()];
    let names = [c"input_ids".as_ptr(), c"attention_mask".as_ptr()];
    let output_names = [c"logits".as_ptr()];
    let mut output = Output(std::ptr::null_mut());
    // SAFETY: ORT explicitly permits concurrent Run on one immutable session:
    // https://onnxruntime.ai/docs/reference/high-level-design.html
    // Only CPU sessions enter this adapter. DirectML uses ort's exclusive safe
    // Run API. The session borrow outlives every local value; tensors, names,
    // null RunOptions and output storage are per-call. Nothing else is mutated.
    checked(unsafe {
        (ort::api().Run)(
            session.ptr().cast_mut(),
            std::ptr::null(),
            names.as_ptr(),
            inputs.as_ptr(),
            2,
            output_names.as_ptr(),
            1,
            &mut output.0,
        )
    })?;
    ensure!(!output.0.is_null(), "ORT returned no student logit");
    let mut shape = Shape(std::ptr::null_mut());
    // SAFETY: successful Run returned this owned output. The metadata guard
    // releases its allocation on every exit path, including API errors.
    checked(unsafe { (ort::api().GetTensorTypeAndShape)(output.0, &mut shape.0) })?;
    ensure!(!shape.0.is_null(), "ORT returned no student output shape");
    let mut count = 0usize;
    let mut dtype = ort::sys::ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_UNDEFINED;
    // SAFETY: shape is valid owned metadata from the call above; destinations
    // are writable local scalar storage and are not shared across calls.
    checked(unsafe { (ort::api().GetTensorShapeElementCount)(shape.0, &mut count) })?;
    // SAFETY: same metadata and per-call destination as above.
    checked(unsafe { (ort::api().GetTensorElementType)(shape.0, &mut dtype) })?;
    ensure!(
        count == 1
            && dtype == ort::sys::ONNXTensorElementDataType::ONNX_TENSOR_ELEMENT_DATA_TYPE_FLOAT,
        "student output must contain one fp32 logit"
    );
    let mut data = std::ptr::null_mut();
    // SAFETY: this CPU-only Run output is a dense fp32 tensor, confirmed above.
    // The getter does not modify its data; output remains owned until return.
    checked(unsafe { (ort::api().GetTensorMutableData)(output.0, &mut data) })?;
    ensure!(!data.is_null(), "ORT returned no student output data");
    // SAFETY: one CPU-accessible, naturally aligned fp32 element, as verified
    // above. Copy it before either the output or the session borrow ends.
    let logit = unsafe { *data.cast::<f32>() };
    ensure!(logit.is_finite(), "student returned nonfinite logit");
    Ok(1.0 / (1.0 + (-f64::from(logit)).exp()))
}
