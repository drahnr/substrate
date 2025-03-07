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

use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_quote, ItemImpl, ItemTrait, Result};

use crate::{
	common::CHANGED_IN_ATTRIBUTE,
	utils::{
		extract_impl_trait, filter_cfg_attributes, generate_crate_access,
		generate_runtime_mod_name_for_trait, get_doc_literals, RequireQualifiedTraitPath,
	},
};

/// Get the type parameter argument without lifetime or mutability
/// of a runtime metadata function.
///
/// In the following example, both the `AccountId` and `Index` generic
/// type parameters must implement `scale_info::TypeInfo` because they
/// are added into the metadata using `scale_info::meta_type`.
///
/// ```ignore
/// trait ExampleAccountNonceApi<AccountId, Index> {
///   fn account_nonce<'a>(account: &'a AccountId) -> Index;
/// }
/// ```
///
/// Instead of returning `&'a AccountId` for the first parameter, this function
/// returns `AccountId` to place bounds around it.
fn get_type_param(ty: &syn::Type) -> syn::Type {
	// Remove the lifetime and mutability of the type T to
	// place bounds around it.
	let ty_elem = match &ty {
		syn::Type::Reference(reference) => &reference.elem,
		syn::Type::Ptr(ptr) => &ptr.elem,
		syn::Type::Slice(slice) => &slice.elem,
		syn::Type::Array(arr) => &arr.elem,
		_ => ty,
	};

	ty_elem.clone()
}

/// Extract the documentation from the provided attributes.
///
/// It takes into account the `no-metadata-docs` feature.
fn collect_docs(attrs: &[syn::Attribute], crate_: &TokenStream2) -> TokenStream2 {
	if cfg!(feature = "no-metadata-docs") {
		quote!(#crate_::vec![])
	} else {
		let docs = get_doc_literals(&attrs);
		quote!(#crate_::vec![ #( #docs, )* ])
	}
}

/// Generate the runtime metadata of the provided trait.
///
/// The metadata is exposed as a generic function on the hidden module
/// of the trait generated by the `decl_runtime_apis`.
pub fn generate_decl_runtime_metadata(decl: &ItemTrait) -> TokenStream2 {
	let crate_ = generate_crate_access();
	let mut methods = Vec::new();

	// Ensure that any function parameter that relies on the `BlockT` bounds
	// also has `TypeInfo + 'static` bounds (required by `scale_info::meta_type`).
	//
	// For example, if a runtime API defines a method that has an input:
	// `fn func(input: <Block as BlockT>::Header)`
	// then the runtime metadata will imply `<Block as BlockT>::Header: TypeInfo + 'static`.
	//
	// This restricts the bounds at the metadata level, without needing to modify the `BlockT`
	// itself, since the concrete implementations are already satisfying `TypeInfo`.
	let mut where_clause = Vec::new();
	for item in &decl.items {
		// Collect metadata for methods only.
		let syn::TraitItem::Fn(method) = item else {
			continue
		};

		// Collect metadata only for the latest methods.
		let is_changed_in =
			method.attrs.iter().any(|attr| attr.path().is_ident(CHANGED_IN_ATTRIBUTE));
		if is_changed_in {
			continue
		}

		let mut inputs = Vec::new();
		let signature = &method.sig;
		for input in &signature.inputs {
			// Exclude `self` from metadata collection.
			let syn::FnArg::Typed(typed) = input else {
				continue
			};

			let pat = &typed.pat;
			let name = quote!(#pat).to_string();
			let ty = &typed.ty;

			where_clause.push(get_type_param(ty));

			inputs.push(quote!(
				#crate_::metadata_ir::RuntimeApiMethodParamMetadataIR {
					name: #name,
					ty: #crate_::scale_info::meta_type::<#ty>(),
				}
			));
		}

		let output = match &signature.output {
			syn::ReturnType::Default => quote!(#crate_::scale_info::meta_type::<()>()),
			syn::ReturnType::Type(_, ty) => {
				where_clause.push(get_type_param(ty));
				quote!(#crate_::scale_info::meta_type::<#ty>())
			},
		};

		// String method name including quotes for constructing `v15::RuntimeApiMethodMetadata`.
		let method_name = signature.ident.to_string();
		let docs = collect_docs(&method.attrs, &crate_);

		// Include the method metadata only if its `cfg` features are enabled.
		let attrs = filter_cfg_attributes(&method.attrs);
		methods.push(quote!(
			#( #attrs )*
			#crate_::metadata_ir::RuntimeApiMethodMetadataIR {
				name: #method_name,
				inputs: #crate_::vec![ #( #inputs, )* ],
				output: #output,
				docs: #docs,
			}
		));
	}

	let trait_name_ident = &decl.ident;
	let trait_name = trait_name_ident.to_string();
	let docs = collect_docs(&decl.attrs, &crate_);
	let attrs = filter_cfg_attributes(&decl.attrs);
	// The trait generics where already extended with `Block: BlockT`.
	let mut generics = decl.generics.clone();
	for generic_param in generics.params.iter_mut() {
		let syn::GenericParam::Type(ty) = generic_param else {
			continue
		};

		// Default type parameters are not allowed in functions.
		ty.eq_token = None;
		ty.default = None;
	}

	let where_clause = where_clause
		.iter()
		.map(|ty| quote!(#ty: #crate_::scale_info::TypeInfo + 'static));

	quote!(
		#( #attrs )*
		#[inline(always)]
		pub fn runtime_metadata #generics () -> #crate_::metadata_ir::RuntimeApiMetadataIR
		where #( #where_clause, )*
		{
			#crate_::metadata_ir::RuntimeApiMetadataIR {
				name: #trait_name,
				methods: #crate_::vec![ #( #methods, )* ],
				docs: #docs,
			}
		}
	)
}

/// Implement the `runtime_metadata` function on the runtime that
/// generates the metadata for the given traits.
///
/// The metadata of each trait is extracted from the generic function
/// exposed by `generate_decl_runtime_metadata`.
pub fn generate_impl_runtime_metadata(impls: &[ItemImpl]) -> Result<TokenStream2> {
	if impls.is_empty() {
		return Ok(quote!())
	}

	let crate_ = generate_crate_access();

	// Get the name of the runtime for which the traits are implemented.
	let runtime_name = &impls
		.get(0)
		.expect("Traits should contain at least one implementation; qed")
		.self_ty;

	let mut metadata = Vec::new();

	for impl_ in impls {
		let mut trait_ = extract_impl_trait(&impl_, RequireQualifiedTraitPath::Yes)?.clone();

		// Implementation traits are always references with a path `impl client::Core<generics> ...`
		// The trait name is the last segment of this path.
		let trait_name_ident = &trait_
			.segments
			.last()
			.as_ref()
			.expect("Trait path should always contain at least one item; qed")
			.ident;

		// Extract the generics from the trait to pass to the `runtime_metadata`
		// function on the hidden module.
		let generics = trait_
			.segments
			.iter()
			.find_map(|segment| {
				if let syn::PathArguments::AngleBracketed(generics) = &segment.arguments {
					Some(generics.clone())
				} else {
					None
				}
			})
			.expect("Trait path should always contain at least one generic parameter; qed");

		let mod_name = generate_runtime_mod_name_for_trait(&trait_name_ident);
		// Get absolute path to the `runtime_decl_for_` module by replacing the last segment.
		if let Some(segment) = trait_.segments.last_mut() {
			*segment = parse_quote!(#mod_name);
		}

		let attrs = filter_cfg_attributes(&impl_.attrs);
		metadata.push(quote!(
			#( #attrs )*
			#trait_::runtime_metadata::#generics()
		));
	}

	// Each runtime must expose the `runtime_metadata()` to fetch the runtime API metadata.
	// The function is implemented by calling `impl_runtime_apis!`.
	//
	// However, the `construct_runtime!` may be called without calling `impl_runtime_apis!`.
	// Rely on the `Deref` trait to differentiate between a runtime that implements
	// APIs (by macro impl_runtime_apis!) and a runtime that is simply created (by macro
	// construct_runtime!).
	//
	// Both `InternalConstructRuntime` and `InternalImplRuntimeApis` expose a `runtime_metadata()`
	// function. `InternalConstructRuntime` is implemented by the `construct_runtime!` for Runtime
	// references (`& Runtime`), while `InternalImplRuntimeApis` is implemented by the
	// `impl_runtime_apis!` for Runtime (`Runtime`).
	//
	// Therefore, the `Deref` trait will resolve the `runtime_metadata` from `impl_runtime_apis!`
	// when both macros are called; and will resolve an empty `runtime_metadata` when only the
	// `construct_runtime!` is called.

	Ok(quote!(
		#[doc(hidden)]
		trait InternalImplRuntimeApis {
			#[inline(always)]
			fn runtime_metadata(&self) -> #crate_::vec::Vec<#crate_::metadata_ir::RuntimeApiMetadataIR> {
				#crate_::vec![ #( #metadata, )* ]
			}
		}
		#[doc(hidden)]
		impl InternalImplRuntimeApis for #runtime_name {}
	))
}
