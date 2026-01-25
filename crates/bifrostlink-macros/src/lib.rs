use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{
	parse::Parser, parse_macro_input, punctuated::Punctuated, spanned::Spanned, Error, Expr,
	ExprLit, FnArg, Ident, ImplItem, ItemImpl, Lit, Meta, MetaNameValue, Pat, ReturnType, Token,
	Type,
};

fn int_value(nv: &MetaNameValue, expected: &str) -> syn::Result<u16> {
	if !nv.path.is_ident(expected) {
		return Err(Error::new(
			nv.path.span(),
			format!("expected `{expected} = <int>`"),
		));
	}
	match &nv.value {
		Expr::Lit(ExprLit {
			lit: Lit::Int(i), ..
		}) => i.base10_parse::<u16>(),
		other => Err(Error::new(other.span(), "expected integer literal")),
	}
}

fn snake_to_pascal(name: &str) -> String {
	let mut out = String::new();
	for part in name.split('_') {
		let mut chars = part.chars();
		if let Some(first) = chars.next() {
			out.extend(first.to_uppercase());
			out.push_str(chars.as_str());
		}
	}
	out
}

fn expand(ns: u16, item: ItemImpl) -> syn::Result<TokenStream> {
	let self_ty = &item.self_ty;
	let self_ident = match &**self_ty {
		Type::Path(p) => p
			.path
			.segments
			.last()
			.map(|s| s.ident.clone())
			.ok_or_else(|| Error::new(self_ty.span(), "expected a named type"))?,
		_ => return Err(Error::new(self_ty.span(), "expected a named type")),
	};
	let client_ident = Ident::new(&format!("{self_ident}Client"), self_ident.span());

	let mut generated = Vec::new();
	let mut client_methods = Vec::new();
	let mut registrations = Vec::new();

	for impl_item in &item.items {
		let ImplItem::Fn(method) = impl_item else {
			continue;
		};

		let Some(id_attr) = method.attrs.iter().find(|a| a.path().is_ident("endpoints")) else {
			continue;
		};
		let metas = id_attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
		let id = metas
			.iter()
			.find_map(|m| match m {
				Meta::NameValue(nv) if nv.path.is_ident("id") => Some(int_value(nv, "id")),
				_ => None,
			})
			.ok_or_else(|| Error::new(id_attr.span(), "missing `id = <int>` argument"))??;
		let cancel = metas
			.iter()
			.any(|m| matches!(m, Meta::Path(p) if p.is_ident("cancel")));
		let endpoint_id: u16 = (ns << 8) | id;

		let method_name = &method.sig.ident;
		let pascal = snake_to_pascal(&method_name.to_string());
		let request_ident = Ident::new(&pascal, method_name.span());
		let response_ident = Ident::new(&format!("{pascal}Response"), method_name.span());

		let mut field_idents = Vec::new();
		let mut field_types = Vec::new();
		for arg in &method.sig.inputs {
			let FnArg::Typed(pat_type) = arg else {
				continue;
			};
			let Pat::Ident(pat_ident) = &*pat_type.pat else {
				return Err(Error::new(
					pat_type.pat.span(),
					"endpoint arguments must be plain identifiers",
				));
			};
			field_idents.push(pat_ident.ident.clone());
			field_types.push((*pat_type.ty).clone());
		}

		let ret_ty: Type = match &method.sig.output {
			ReturnType::Type(_, ty) => (**ty).clone(),
			ReturnType::Default => syn::parse_quote!(()),
		};

		let maybe_await = method.sig.asyncness.map(|_| quote!(.await));
		let cancellable = cancel;

		generated.push(quote! {
			#[derive(Serialize, Deserialize)]
			struct #request_ident {
				#( #field_idents: #field_types ),*
			}

			#[derive(Serialize, Deserialize)]
			struct #response_ident(#ret_ty);

			::bifrostlink::request!((#endpoint_id) #request_ident => #response_ident);
		});

		registrations.push(quote! {
			{
				let v = v.clone();
				rpc.register_request_handler::<#request_ident, _>(#cancellable, move |_addr, req| {
					let v = v.clone();
					async move {
						Ok(#response_ident(
							v.#method_name( #( req.#field_idents ),* ) #maybe_await
						))
					}
				});
			}
		});

		client_methods.push(quote! {
			pub async fn #method_name(
				&self,
				#( #field_idents: #field_types ),*
			) -> Result<#ret_ty, C::Error> {
				Ok(self
					.0
					.request(self.1.clone(), #request_ident { #( #field_idents ),* })
					.await?
					.0)
			}
		});
	}

	let mut clean = item.clone();
	for impl_item in &mut clean.items {
		if let ImplItem::Fn(method) = impl_item {
			method.attrs.retain(|a| !a.path().is_ident("endpoints"));
		}
	}

	let (impl_generics, _ty_generics, where_clause) = item.generics.split_for_impl();

	Ok(quote! {
		#clean

		#( #generated )*

		impl #impl_generics #self_ty #where_clause {
			pub fn register_endpoints<C: Config>(self, rpc: &mut ::bifrostlink::Rpc<C>) {
				let v = ::std::sync::Arc::new(self);
				#( #registrations )*
			}
		}

		pub struct #client_ident<C: ::bifrostlink::Config>(pub ::bifrostlink::Rpc<C>, pub C::Address);
		impl<C: ::bifrostlink::Config> #client_ident<C> {
			#( #client_methods )*
		}
	})
}

#[proc_macro_attribute]
pub fn endpoints(
	attrs: proc_macro::TokenStream,
	body: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
	let item = parse_macro_input!(body as ItemImpl);

	let parser = Punctuated::<Meta, Token![,]>::parse_terminated;
	let metas = match parser.parse(attrs) {
		Ok(m) => m,
		Err(e) => return e.to_compile_error().into(),
	};
	let ns = match metas.iter().find_map(|m| match m {
		Meta::NameValue(nv) if nv.path.is_ident("ns") => Some(int_value(nv, "ns")),
		_ => None,
	}) {
		Some(Ok(ns)) => ns,
		Some(Err(e)) => return e.to_compile_error().into(),
		None => {
			return Error::new(Span::call_site(), "missing `ns = <int>` argument")
				.to_compile_error()
				.into()
		}
	};

	match expand(ns, item) {
		Ok(ts) => ts.into(),
		Err(e) => e.to_compile_error().into(),
	}
}
