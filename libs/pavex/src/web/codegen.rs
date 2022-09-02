use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use bimap::BiHashMap;
use cargo_manifest::{Dependency, DependencyDetail, Edition};
use guppy::graph::PackageSource;
use guppy::{PackageId, Version};
use indexmap::{IndexMap, IndexSet};
use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use syn::{parse_quote, ItemFn, ItemStruct};

use crate::language::ResolvedPath;
use crate::language::{Callable, ResolvedType};
use crate::web::app::{GENERATED_APP_PACKAGE_ID, STD_PACKAGE_ID};
use crate::web::application_state_call_graph::ApplicationStateCallGraph;
use crate::web::dependency_graph::DependencyGraphNode;
use crate::web::handler_call_graph::{codegen, HandlerCallGraph};

pub(crate) fn codegen_app(
    router: &BTreeMap<String, Callable>,
    handler_call_graphs: &IndexMap<ResolvedPath, HandlerCallGraph>,
    application_state_call_graph: &ApplicationStateCallGraph,
    request_scoped_framework_bindings: &BiHashMap<Ident, ResolvedType>,
    package_id2name: &BiHashMap<&'_ PackageId, String>,
) -> Result<TokenStream, anyhow::Error> {
    let define_application_state = define_application_state(
        &application_state_call_graph.runtime_singleton_bindings,
        package_id2name,
    );
    let application_state_init =
        get_application_state_init(application_state_call_graph, package_id2name)?;
    let define_server_state = define_server_state();

    let handler_functions: HashMap<_, _> = handler_call_graphs
        .into_iter()
        .map(|(path, call_graph)| {
            let code = codegen(call_graph, package_id2name)?;
            Ok::<_, anyhow::Error>((path, (code, call_graph.input_parameter_types.clone())))
        })
        // TODO: wasteful
        .collect::<Result<HashMap<_, _>, _>>()?
        .into_iter()
        .enumerate()
        .map(|(i, (path, (mut function, parameter_bindings)))| {
            // Ensure that all handler functions have a unique name.
            function.sig.ident = format_ident!("route_handler_{}", i);
            (path, (function, parameter_bindings))
        })
        .collect::<HashMap<_, _>>();

    // TODO: enforce that handlers have the right signature
    // TODO: enforce that the only required input is a Request type of some kind
    let mut route_id2path = BiHashMap::new();
    let mut route_id2handler = HashMap::new();
    for (route_id, (path, handler)) in router.iter().enumerate() {
        route_id2path.insert(route_id as u32, path.clone());
        route_id2handler.insert(
            route_id as u32,
            handler_functions[&handler.callable_fq_path].to_owned(),
        );
    }

    let router_init = get_router_init(&route_id2path);
    let route_request = get_request_dispatcher(
        &route_id2handler,
        &application_state_call_graph.runtime_singleton_bindings,
        request_scoped_framework_bindings,
    );
    let handlers = handler_functions.values().map(|(function, _)| function);
    let entrypoint = server_startup();
    let code = quote! {
        #define_server_state
        #define_application_state
        #application_state_init
        #entrypoint
        #router_init
        #route_request
        #(#handlers)*
    };
    Ok(code)
}

fn server_startup() -> ItemFn {
    parse_quote! {
        pub async fn run(
            server_builder: pavex_runtime::hyper::server::Builder<pavex_runtime::hyper::server::conn::AddrIncoming>,
            application_state: ApplicationState
        ) -> Result<(), anyhow::Error> {
            let server_state = std::sync::Arc::new(ServerState {
                router: build_router()?,
                application_state
            });
            let make_service = pavex_runtime::hyper::service::make_service_fn(move |_| {
                let server_state = server_state.clone();
                async move {
                    Ok::<_, pavex_runtime::hyper::Error>(pavex_runtime::hyper::service::service_fn(move |request| {
                        let server_state = server_state.clone();
                        async move { Ok::<_, pavex_runtime::hyper::Error>(route_request(request, server_state)) }
                    }))
                }
            });
            server_builder.serve(make_service).await.map_err(Into::into)
        }
    }
}

fn define_application_state(
    runtime_singletons: &BiHashMap<Ident, ResolvedType>,
    package_id2name: &BiHashMap<&'_ PackageId, String>,
) -> ItemStruct {
    let singleton_fields = runtime_singletons.iter().map(|(field_name, type_)| {
        let field_type = type_.syn_type(package_id2name);
        quote! { #field_name: #field_type }
    });
    parse_quote! {
        pub struct ApplicationState {
            #(#singleton_fields),*
        }
    }
}

fn define_server_state() -> ItemStruct {
    parse_quote! {
        struct ServerState {
            router: pavex_runtime::routing::Router<u32>,
            application_state: ApplicationState
        }
    }
}

fn get_application_state_init(
    application_state_call_graph: &ApplicationStateCallGraph,
    package_id2name: &BiHashMap<&'_ PackageId, String>,
) -> Result<ItemFn, anyhow::Error> {
    let mut function = application_state_call_graph.codegen(package_id2name)?;
    function.sig.ident = format_ident!("build_application_state");
    Ok(function)
}

fn get_router_init(route_id2path: &BiHashMap<u32, String>) -> ItemFn {
    let mut router_init = quote! {
        let mut router = pavex_runtime::routing::Router::new();
    };
    for (route_id, path) in route_id2path {
        router_init = quote! {
            #router_init
            router.insert(#path, #route_id)?;
        };
    }
    parse_quote! {
        fn build_router() -> Result<pavex_runtime::routing::Router<u32>, pavex_runtime::routing::InsertError> {
            #router_init
            Ok(router)
        }
    }
}

fn get_request_dispatcher(
    route_id2handler: &HashMap<u32, (ItemFn, IndexSet<ResolvedType>)>,
    singleton_bindings: &BiHashMap<Ident, ResolvedType>,
    request_scoped_bindings: &BiHashMap<Ident, ResolvedType>,
) -> ItemFn {
    let mut route_dispatch_table = quote! {};

    for (route_id, (handler, handler_input_types)) in route_id2handler {
        let handler_function_name = &handler.sig.ident;
        let input_parameters = handler_input_types.iter().map(|type_| {
            if let Some(field_name) = singleton_bindings.get_by_right(type_) {
                quote! {
                    server_state.application_state.#field_name.clone()
                }
            } else {
                let field_name = request_scoped_bindings.get_by_right(type_).unwrap();
                quote! {
                    #field_name
                }
            }
        });
        route_dispatch_table = quote! {
            #route_dispatch_table
            #route_id => #handler_function_name(#(#input_parameters),*),
        }
    }

    parse_quote! {
        fn route_request(request: pavex_runtime::http::Request<pavex_runtime::hyper::body::Body>, server_state: std::sync::Arc<ServerState>) -> pavex_runtime::http::Response<pavex_runtime::hyper::body::Body> {
            let route_id = server_state.router.at(request.uri().path()).expect("Failed to match incoming request path");
            match route_id.value {
                #route_dispatch_table
                _ => panic!("This is a bug, no route registered for a route id"),
            }
        }
    }
}

pub(crate) fn codegen_manifest<'a>(
    package_graph: &guppy::graph::PackageGraph,
    handler_call_graphs: &'a IndexMap<ResolvedPath, HandlerCallGraph>,
    application_state_call_graph: &'a ApplicationStateCallGraph,
) -> (cargo_manifest::Manifest, BiHashMap<&'a PackageId, String>) {
    let (dependencies, package_ids2deps) = compute_dependencies(
        package_graph,
        handler_call_graphs,
        application_state_call_graph,
    );
    let manifest = cargo_manifest::Manifest {
        dependencies: Some(dependencies),
        package: Some(cargo_manifest::Package {
            // TODO: this should be configurable
            name: "application".to_string(),
            edition: Edition::E2021,
            version: "0.1.0".to_string(),
            build: None,
            workspace: None,
            authors: vec![],
            links: None,
            description: None,
            homepage: None,
            documentation: None,
            readme: None,
            keywords: vec![],
            categories: vec![],
            license: None,
            license_file: None,
            repository: None,
            metadata: None,
            default_run: None,
            autobins: false,
            autoexamples: false,
            autotests: false,
            autobenches: false,
            publish: Default::default(),
            resolver: None,
        }),
        workspace: None,
        dev_dependencies: None,
        build_dependencies: None,
        target: None,
        features: None,
        bin: None,
        bench: None,
        test: None,
        example: None,
        patch: None,
        lib: None,
        profile: None,
        badges: None,
    };
    (manifest, package_ids2deps)
}

fn compute_dependencies<'a>(
    package_graph: &guppy::graph::PackageGraph,
    handler_call_graphs: &'a IndexMap<ResolvedPath, HandlerCallGraph>,
    application_state_call_graph: &'a ApplicationStateCallGraph,
) -> (
    BTreeMap<String, Dependency>,
    BiHashMap<&'a PackageId, String>,
) {
    let package_ids = collect_package_ids(handler_call_graphs, application_state_call_graph);
    let mut external_crates: IndexMap<&str, IndexSet<(&Version, &PackageId, Option<PathBuf>)>> =
        Default::default();
    let workspace_root = package_graph.workspace().root();
    for package_id in &package_ids {
        if package_id.repr() != GENERATED_APP_PACKAGE_ID && package_id.repr() != STD_PACKAGE_ID {
            let metadata = package_graph.metadata(package_id).unwrap();
            let path = match metadata.source() {
                PackageSource::Workspace(p) | PackageSource::Path(p) => {
                    let path = if p.is_relative() {
                        workspace_root.join(p)
                    } else {
                        p.to_owned()
                    };
                    Some(path.into_std_path_buf())
                }
                // TODO: handle external deps
                PackageSource::External(_) => None,
            };
            external_crates.entry(metadata.name()).or_default().insert((
                metadata.version(),
                package_id,
                path,
            ));
        }
    }
    let mut dependencies = BTreeMap::new();
    let mut package_ids2dependency_name = BiHashMap::new();
    for (name, versions) in external_crates {
        if versions.len() == 1 {
            let (version, package_id, path) = versions.into_iter().next().unwrap();
            let dependency = if let Some(path) = path {
                cargo_manifest::Dependency::Detailed(DependencyDetail {
                    package: Some(name.to_string()),
                    version: Some(version.to_string()),
                    path: Some(path.to_string_lossy().to_string()),
                    ..DependencyDetail::default()
                })
            } else {
                cargo_manifest::Dependency::Simple(version.to_string())
            };
            dependencies.insert(name.to_owned(), dependency);
            package_ids2dependency_name.insert(package_id, name.to_owned());
        } else {
            for (i, (version, package_id, path)) in versions.into_iter().enumerate() {
                let rename = format!("{name}_{i}");
                let dependency = cargo_manifest::Dependency::Detailed(DependencyDetail {
                    package: Some(name.to_string()),
                    version: Some(version.to_string()),
                    path: path.map(|p| p.to_string_lossy().to_string()),
                    ..DependencyDetail::default()
                });
                dependencies.insert(rename.clone(), dependency);
                package_ids2dependency_name.insert(package_id, rename);
            }
        }
    }
    (dependencies, package_ids2dependency_name)
}

fn collect_package_ids<'a>(
    handler_call_graphs: &'a IndexMap<ResolvedPath, HandlerCallGraph>,
    application_state_call_graph: &'a ApplicationStateCallGraph,
) -> IndexSet<&'a PackageId> {
    let mut package_ids = IndexSet::new();
    for node in application_state_call_graph.call_graph.node_weights() {
        match node {
            DependencyGraphNode::Compute(c) => {
                collect_callable_package_ids(&mut package_ids, c);
            }
            DependencyGraphNode::Type(t) => {
                if let Some(c) = application_state_call_graph.constructors.get(t) {
                    collect_callable_package_ids(&mut package_ids, c)
                }
            }
        }
    }
    for handler_call_graph in handler_call_graphs.values() {
        for node in handler_call_graph.call_graph.node_weights() {
            match node {
                DependencyGraphNode::Compute(c) => {
                    collect_callable_package_ids(&mut package_ids, c);
                }
                DependencyGraphNode::Type(t) => {
                    if let Some(c) = handler_call_graph.constructors.get(t) {
                        collect_callable_package_ids(&mut package_ids, c)
                    }
                }
            }
        }
    }
    package_ids
}

fn collect_callable_package_ids<'a>(package_ids: &mut IndexSet<&'a PackageId>, c: &'a Callable) {
    // What about the generic parameters of the callable?
    package_ids.insert(&c.callable_fq_path.package_id);
    for input in &c.inputs {
        collect_type_package_ids(package_ids, input);
    }
    collect_type_package_ids(package_ids, &c.output_fq_path);
}

fn collect_type_package_ids<'a>(package_ids: &mut IndexSet<&'a PackageId>, t: &'a ResolvedType) {
    package_ids.insert(&t.package_id);
    for generic in &t.generic_arguments {
        collect_type_package_ids(package_ids, generic);
    }
}