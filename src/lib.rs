#![warn(unused_crate_dependencies, unused_extern_crates)]
#![warn(unreachable_pub)]

use gloo_utils::format::JsValueSerdeExt;
use js_sys::Map;
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

mod abi;
mod barretenberg;
mod compression;
mod execute;
mod foreign_calls;
mod js_transforms;
mod logging;
mod public_witness;

pub use abi::{abi_decode, abi_encode};
pub use compression::{compress_witness, decompress_witness};
pub use execute::execute_circuit;
pub use logging::{init_log_level, LogLevel};
pub use public_witness::{get_public_parameters_witness, get_public_witness, get_return_witness};

#[derive(Serialize, Deserialize)]
pub struct BuildInfo {
    git_hash: &'static str,
    version: &'static str,
    dirty: &'static str,
}

const BUILD_INFO: BuildInfo = BuildInfo {
    git_hash: env!("GIT_COMMIT"),
    version: env!("CARGO_PKG_VERSION"),
    dirty: env!("GIT_DIRTY"),
};

#[wasm_bindgen(js_name = buildInfo)]
pub fn build_info() -> JsValue {
    console_error_panic_hook::set_once();
    <JsValue as JsValueSerdeExt>::from_serde(&BUILD_INFO).unwrap()
}

#[wasm_bindgen(typescript_custom_section)]
const WITNESS_MAP: &'static str = r#"
// Map from witness index to hex string value of witness.
export type WitnessMap = Map<number, string>;
"#;

// WitnessMap
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = Map, js_name = "WitnessMap", typescript_type = "WitnessMap")]
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub type JsWitnessMap;

    #[wasm_bindgen(constructor, js_class = "Map")]
    pub fn new() -> JsWitnessMap;

}

impl Default for JsWitnessMap {
    fn default() -> Self {
        Self::new()
    }
}
