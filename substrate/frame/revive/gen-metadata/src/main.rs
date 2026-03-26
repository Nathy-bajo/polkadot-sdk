// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Generates `revive_chain.scale` — the runtime metadata file consumed by the
//! `#[subxt::subxt]` macro in `pallet-revive-eth-rpc`.
//!
//! Run this whenever `revive-dev-runtime`'s API changes:
//!
//! ```text
//! cargo run -p revive-gen-metadata
//! ```

use std::fs;

fn main() {
	let out_path =
		std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../rpc/revive_chain.scale");

	let mut ext = sp_io::TestExternalities::new(Default::default());
	ext.execute_with(|| {
		let metadata = revive_dev_runtime::Runtime::metadata_at_version(16)
			.expect("metadata v16 must be supported; qed");
		let bytes: &[u8] = &metadata;
		fs::write(&out_path, bytes).expect("failed to write revive_chain.scale");
	});

	println!("Written to {}", out_path.display());
}
