//! Graph command: Deep code relationship analysis
//!
//! Subcommands:
//! - summary: Generate AI-readable summary of the code graph
//! - type: Type relationship analysis (TypeFlowGraph)
//! - flow: Data flow analysis (provenance/impact/source/sink)
//! - borrow: Borrow analysis (conflicts, use-after-move)
//! - lock: Lock analysis (critical sections, optimization suggestions)
//! - cascade: Change impact analysis (ripple effects)
//!
//! For basic relationship queries (callers, callees, uses, etc.), use RyoQL:
//!   ryo query 'kind: Function | match: { name: foo } | resolve: { kind: Callers }'

use crate::util::load_context;
use anyhow::Result;
use clap::{Args, Subcommand};
use colored::Colorize;
use ryo_analysis::{AnalysisContext, BorrowKind, LockGranularityAnalyzerV2, SymbolId, VarId};
use ryo_app::{GraphApi, Project, SummaryBuilder};
use std::path::PathBuf;

/// Resolve a symbol from either name or id
///
/// If `id` is provided, parse it as SymbolId directly.
/// Otherwise, search by name.
fn resolve_symbol_renamed(
    ctx: &AnalysisContext,
    name: Option<&str>,
    id: Option<&str>,
) -> Result<(SymbolId, String)> {
    // Try --id first
    if let Some(id_str) = id {
        let symbol_id = SymbolId::parse(id_str).ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid SymbolId format: '{}'. Expected format: '165v1'",
                id_str
            )
        })?;
        let display_name = ctx
            .registry
            .resolve(symbol_id)
            .map(|p| p.name().to_string())
            .unwrap_or_else(|| id_str.to_string());
        return Ok((symbol_id, display_name));
    }

    // Fall back to name search
    let name = name.ok_or_else(|| anyhow::anyhow!("Either name or --id must be provided"))?;
    let symbol_id = ctx
        .registry
        .iter()
        .find(|(sid, _)| {
            ctx.registry
                .resolve(*sid)
                .map(|p| p.name() == name)
                .unwrap_or(false)
        })
        .map(|(sid, _)| sid)
        .ok_or_else(|| anyhow::anyhow!("Symbol '{}' not found", name))?;

    Ok((symbol_id, name.to_string()))
}

#[derive(Args)]
pub struct GraphArgs {
    #[command(subcommand)]
    pub command: GraphCommand,
}

#[derive(Subcommand)]
pub enum GraphCommand {
    /// Generate AI-readable summary of the code graph
    Summary {
        /// Detailed output with file paths and type breakdown
        #[arg(long)]
        detailed: bool,

        /// Compact output for minimal display
        #[arg(long)]
        compact: bool,

        /// Maximum items per section
        #[arg(long)]
        max_items: Option<usize>,

        /// Output file (stdout if not specified)
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Target directory
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },

    /// Type relationship analysis using TypeFlowGraph
    ///
    /// Analyzes type definitions → usage relationships with O(1) impact analysis
    /// via inverted index.
    #[command(name = "type")]
    TypeAnalysis {
        /// Type name to analyze (can be omitted if --id is provided)
        #[arg(required_unless_present = "id")]
        name: Option<String>,

        /// Lookup by SymbolId directly (e.g., "165v1")
        #[arg(long)]
        id: Option<String>,

        /// Analysis mode: definition, usage, impact
        #[arg(long, default_value = "impact")]
        mode: TypeAnalysisMode,

        /// Maximum depth for traversal
        #[arg(long, default_value = "3")]
        depth: usize,

        /// Target directory
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },

    /// Data flow analysis (provenance and impact tracking)
    ///
    /// Tracks data provenance (where values come from) and impact
    /// (where values flow to). Identifies sources and sinks.
    Flow {
        /// Function/method name containing variables to trace
        /// (not a type name; can be omitted if --id is provided)
        #[arg(required_unless_present = "id")]
        name: Option<String>,

        /// Lookup by SymbolId directly (e.g., "165v1")
        #[arg(long)]
        id: Option<String>,

        /// Analysis mode: provenance, impact, chain, sources, sinks
        #[arg(long, default_value = "chain")]
        mode: FlowAnalysisMode,

        /// Maximum depth for traversal
        #[arg(long, default_value = "5")]
        depth: usize,

        /// Target directory
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },

    /// Borrow analysis (conflict detection, use-after-move)
    ///
    /// Tracks active borrows and detects potential conflicts or
    /// use-after-move errors.
    Borrow {
        /// Function/method name containing variables to analyze
        /// (not a type name; can be omitted if --id is provided)
        #[arg(required_unless_present = "id")]
        name: Option<String>,

        /// Lookup by SymbolId directly (e.g., "165v1")
        #[arg(long)]
        id: Option<String>,

        /// Show only conflicts
        #[arg(long)]
        conflicts_only: bool,

        /// Target directory
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },

    /// Lock analysis (critical sections and optimization)
    ///
    /// Analyzes lock usage patterns, critical section sizes,
    /// and provides optimization suggestions.
    Lock {
        /// Lock variable to analyze (e.g., "self.mutex")
        /// (can be omitted if --id is provided)
        #[arg(required_unless_present = "id")]
        name: Option<String>,

        /// Lookup by SymbolId directly (e.g., "165v1")
        #[arg(long)]
        id: Option<String>,

        /// Show optimization suggestions
        #[arg(long)]
        suggest: bool,

        /// Target directory
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },

    /// Change cascade analysis (ripple effects)
    ///
    /// Analyzes the cascading effects of a potential change,
    /// identifying all affected symbols and required additional changes.
    ///
    /// Use `ryo discover` first to find the SymbolId of the target symbol.
    Cascade {
        /// SymbolId to analyze (e.g., "165v1")
        ///
        /// Get this from `ryo discover` output
        #[arg(long, required = true)]
        id: String,

        /// Maximum cascade depth
        #[arg(long, default_value = "3")]
        depth: usize,

        /// Target directory
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },

    /// Call chain analysis (transitive caller/callee traversal)
    ///
    /// Traverses call relationships transitively to find all callers or callees
    /// up to a specified depth. This is the control-flow counterpart to data-flow
    /// analysis.
    ///
    /// Use `ryo discover` first to find the SymbolId of the target symbol.
    Chain {
        /// SymbolId to analyze (e.g., "165v1")
        ///
        /// Get this from `ryo discover` output
        #[arg(long, required = true)]
        id: String,

        /// Traversal mode: callers, callees, users, uses
        #[arg(long, default_value = "callers")]
        mode: ChainMode,

        /// Maximum traversal depth
        #[arg(long, default_value = "5")]
        depth: usize,

        /// Target directory
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
}

/// Type analysis mode
#[derive(Clone, Default, clap::ValueEnum)]
pub enum TypeAnalysisMode {
    /// Show type definition details
    Definition,
    /// Show all usages of this type
    Usage,
    /// Show impact of changing this type
    #[default]
    Impact,
}

/// Flow analysis mode
#[derive(Clone, Default, clap::ValueEnum)]
pub enum FlowAnalysisMode {
    /// Track where values come from
    Provenance,
    /// Track where values flow to
    Impact,
    /// Full chain analysis (provenance + impact)
    #[default]
    Chain,
    /// Find data sources
    Sources,
    /// Find data sinks
    Sinks,
}

/// Chain traversal mode (control-flow direction)
///
/// - `callers`/`callees`: Call chain traversal (CodeGraphV2)
/// - `type-users`/`type-deps`: Type reference chain traversal (TypeFlowGraphV2)
#[derive(Clone, Default, clap::ValueEnum)]
pub enum ChainMode {
    /// Traverse incoming call edges (who calls this function?)
    #[default]
    Callers,
    /// Traverse outgoing call edges (what does this function call?)
    Callees,
    /// Traverse type reference edges: who uses this type? (type → containers)
    TypeUsers,
    /// Traverse type reference edges: what types does this use? (container → types)
    TypeDeps,
}

pub fn execute(args: GraphArgs) -> Result<()> {
    match args.command {
        GraphCommand::Summary {
            detailed,
            compact,
            max_items,
            output,
            path,
        } => {
            use super::{find_project_root, ExecutionMode};

            // Determine execution mode
            let mode = ExecutionMode::from_env();

            match mode {
                ExecutionMode::Client => {
                    let project_path = find_project_root(&path)?;
                    execute_summary_via_server_spawn(
                        &project_path,
                        detailed,
                        compact,
                        max_items,
                        output,
                    )
                }
                ExecutionMode::Standalone => {
                    mode.print_banner();
                    eprintln!("{}", "Loading codebase...".dimmed());
                    execute_summary(&path, detailed, compact, max_items, output)
                }
            }
        }
        GraphCommand::TypeAnalysis {
            name,
            id,
            mode,
            depth,
            path,
        } => {
            use super::{find_project_root, ExecutionMode};

            let exec_mode = ExecutionMode::from_env();
            match exec_mode {
                ExecutionMode::Client => {
                    let project_path = find_project_root(&path)?;
                    execute_type_via_server_spawn(
                        &project_path,
                        name.as_deref(),
                        id.as_deref(),
                        mode,
                        depth,
                    )
                }
                ExecutionMode::Standalone => {
                    exec_mode.print_banner();
                    super::warn_standalone_id(id.as_deref());
                    execute_type_analysis(&path, name.as_deref(), id.as_deref(), mode, depth)
                }
            }
        }
        GraphCommand::Flow {
            name,
            id,
            mode,
            depth,
            path,
        } => {
            use super::{find_project_root, ExecutionMode};

            let exec_mode = ExecutionMode::from_env();
            match exec_mode {
                ExecutionMode::Client => {
                    let project_path = find_project_root(&path)?;
                    execute_flow_via_server_spawn(
                        &project_path,
                        name.as_deref(),
                        id.as_deref(),
                        mode,
                        depth,
                    )
                }
                ExecutionMode::Standalone => {
                    exec_mode.print_banner();
                    super::warn_standalone_id(id.as_deref());
                    execute_flow_analysis(&path, name.as_deref(), id.as_deref(), mode, depth)
                }
            }
        }
        GraphCommand::Borrow {
            name,
            id,
            conflicts_only,
            path,
        } => {
            use super::{find_project_root, ExecutionMode};

            let exec_mode = ExecutionMode::from_env();
            match exec_mode {
                ExecutionMode::Client => {
                    let project_path = find_project_root(&path)?;
                    execute_borrow_via_server_spawn(
                        &project_path,
                        name.as_deref(),
                        id.as_deref(),
                        conflicts_only,
                    )
                }
                ExecutionMode::Standalone => {
                    exec_mode.print_banner();
                    super::warn_standalone_id(id.as_deref());
                    execute_borrow_analysis(&path, name.as_deref(), id.as_deref(), conflicts_only)
                }
            }
        }
        GraphCommand::Lock {
            name,
            id,
            suggest,
            path,
        } => {
            use super::{find_project_root, ExecutionMode};

            let exec_mode = ExecutionMode::from_env();
            match exec_mode {
                ExecutionMode::Client => {
                    let project_path = find_project_root(&path)?;
                    execute_lock_via_server_spawn(
                        &project_path,
                        name.as_deref(),
                        id.as_deref(),
                        suggest,
                    )
                }
                ExecutionMode::Standalone => {
                    exec_mode.print_banner();
                    super::warn_standalone_id(id.as_deref());
                    execute_lock_analysis(&path, name.as_deref(), id.as_deref(), suggest)
                }
            }
        }
        GraphCommand::Cascade { id, depth, path } => {
            use super::{find_project_root, ExecutionMode};

            // Determine execution mode
            let mode = ExecutionMode::from_env();

            match mode {
                ExecutionMode::Client => {
                    let project_path = find_project_root(&path)?;
                    execute_cascade_via_server_spawn(&project_path, &id, depth)
                }
                ExecutionMode::Standalone => {
                    mode.print_banner();
                    super::warn_standalone_id(Some(&id));
                    execute_cascade_analysis(&path, &id, depth)
                }
            }
        }
        GraphCommand::Chain {
            id,
            mode,
            depth,
            path,
        } => {
            use super::{find_project_root, ExecutionMode};

            let exec_mode = ExecutionMode::from_env();

            match exec_mode {
                ExecutionMode::Client => {
                    let project_path = find_project_root(&path)?;
                    execute_chain_via_server_spawn(&project_path, &id, mode, depth)
                }
                ExecutionMode::Standalone => {
                    exec_mode.print_banner();
                    super::warn_standalone_id(Some(&id));
                    execute_chain_analysis(&path, &id, mode, depth)
                }
            }
        }
    }
}

fn build_graph(path: &PathBuf) -> Result<GraphApi> {
    let project =
        Project::load(path).map_err(|e| anyhow::anyhow!("Failed to load project: {}", e))?;
    let loaded = load_context(&project)?;
    let load_time = loaded.load_time;
    Ok(GraphApi::from_context(loaded.take_and_save(), load_time))
}

fn execute_summary(
    path: &PathBuf,
    detailed: bool,
    compact: bool,
    max_items: Option<usize>,
    output: Option<PathBuf>,
) -> Result<()> {
    let graph = build_graph(path)?;
    let build_time = graph.build_time();

    let mut builder = SummaryBuilder::new(&graph);

    if detailed {
        builder = builder.detailed();
    } else if compact {
        builder = builder.compact();
    }

    if let Some(max) = max_items {
        builder = builder.max_items(max);
    }

    let content = builder.build();

    if let Some(output_path) = output {
        std::fs::write(&output_path, &content)?;
        println!(
            "{} {} (build: {:?})",
            "Written to:".green(),
            output_path.display(),
            build_time
        );
    } else {
        println!(
            "{} (build: {:?})",
            "CodeGraph Summary".cyan().bold(),
            build_time
        );
        println!();
        println!("{}", content);
    }

    Ok(())
}

fn format_generics_local(g: &ryo_analysis::detail_store::GenericInfo) -> String {
    let mut parts = Vec::new();
    for lt in &g.lifetimes {
        parts.push(lt.clone());
    }
    for tp in &g.type_params {
        parts.push(tp.clone());
    }
    for (name, ty) in &g.const_params {
        parts.push(format!("const {}: {}", name, ty));
    }
    format!("<{}>", parts.join(", "))
}

fn execute_type_analysis(
    path: &PathBuf,
    name: Option<&str>,
    id: Option<&str>,
    mode: TypeAnalysisMode,
    _depth: usize,
) -> Result<()> {
    let graph = build_graph(path)?;
    let ctx = graph.context();

    let (symbol_id, display_name) = resolve_symbol(ctx, name, id)?;

    // Get full path for SymbolRef format
    let symbol_path = ctx.registry.resolve(symbol_id);
    let symbol_ref = symbol_path
        .map(|p| format!("{:?}@{}", symbol_id, p))
        .unwrap_or_else(|| format!("{:?}", symbol_id));

    println!(
        "{} '{}' ({})",
        "Type Analysis".cyan().bold(),
        display_name.yellow(),
        symbol_ref.dimmed()
    );
    println!();

    let typeflow = &ctx.typeflow_graph;

    match mode {
        TypeAnalysisMode::Definition => {
            if let Some(def_node) = typeflow.definition(symbol_id) {
                if let Some(def_data) = typeflow.get_definition(def_node) {
                    println!("  {} {:?}", "Kind:".cyan(), def_data.kind);
                }
            }
            let ds = &ctx.detail_store;
            if let Some(detail) = ds.struct_(symbol_id) {
                if !detail.generics.is_empty() {
                    println!(
                        "  {} {}",
                        "Generics:".cyan(),
                        format_generics_local(&detail.generics)
                    );
                }
                if !detail.attrs.is_empty() {
                    println!(
                        "  {} {}",
                        "Attributes:".cyan(),
                        detail
                            .attrs
                            .iter()
                            .filter(|a| !a.starts_with("doc"))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                println!("  {} ({})", "Fields:".cyan(), detail.fields.len());
                for f in &detail.fields {
                    let vis = if f.is_public { "pub " } else { "" };
                    println!("    - {}{}: {}", vis, f.name.green(), f.ty);
                }
            } else if let Some(detail) = ds.enum_(symbol_id) {
                if !detail.generics.is_empty() {
                    println!(
                        "  {} {}",
                        "Generics:".cyan(),
                        format_generics_local(&detail.generics)
                    );
                }
                if !detail.attrs.is_empty() {
                    println!(
                        "  {} {}",
                        "Attributes:".cyan(),
                        detail
                            .attrs
                            .iter()
                            .filter(|a| !a.starts_with("doc"))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                println!("  {} ({})", "Variants:".cyan(), detail.variants.len());
                for v in &detail.variants {
                    if v.fields.is_empty() {
                        println!("    - {}", v.name.green());
                    } else {
                        let fields: Vec<String> = v
                            .fields
                            .iter()
                            .map(|f| format!("{}: {}", f.name, f.ty))
                            .collect();
                        println!("    - {} {{ {} }}", v.name.green(), fields.join(", "));
                    }
                }
            } else if let Some(detail) = ds.function(symbol_id) {
                if !detail.generics.is_empty() {
                    println!(
                        "  {} {}",
                        "Generics:".cyan(),
                        format_generics_local(&detail.generics)
                    );
                }
                if !detail.attrs.is_empty() {
                    println!(
                        "  {} {}",
                        "Attributes:".cyan(),
                        detail
                            .attrs
                            .iter()
                            .filter(|a| !a.starts_with("doc"))
                            .cloned()
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                if !detail.params.is_empty() {
                    println!("  {}", "Parameters:".cyan());
                    for p in &detail.params {
                        if !p.is_self {
                            println!("    - {}: {}", p.name.green(), p.ty);
                        }
                    }
                }
                if let Some(ret) = &detail.return_type {
                    println!("  {} {}", "Returns:".cyan(), ret);
                }
            } else if let Some(detail) = ds.trait_(symbol_id) {
                if !detail.generics.is_empty() {
                    println!(
                        "  {} {}",
                        "Generics:".cyan(),
                        format_generics_local(&detail.generics)
                    );
                }
                if !detail.supertraits.is_empty() {
                    println!("  {}", "Supertraits:".cyan());
                    for st in &detail.supertraits {
                        println!("    - {}", st.green());
                    }
                }
                if !detail.methods.is_empty() {
                    println!("  {} ({})", "Methods:".cyan(), detail.methods.len());
                    for m in &detail.methods {
                        println!("    - {}", m.green());
                    }
                }
            } else {
                println!("{}", "  No definition details found".dimmed());
            }
            let usage_count = typeflow.usages(symbol_id).count();
            println!("  {} {}", "Usage count:".cyan().dimmed(), usage_count);
        }
        TypeAnalysisMode::Usage => {
            println!("{}", "Mode: Usage".dimmed());
            let usages: Vec<_> = typeflow.usages(symbol_id).collect();
            println!("  {} {}", "Usage count:".cyan(), usages.len());
            for (i, usage_node) in usages.iter().take(20).enumerate() {
                if let Some(usage_data) = typeflow.get_usage(*usage_node) {
                    let context_str = format!("{:?}", usage_data.context);
                    let ref_kind_str = format!("{:?}", usage_data.ref_kind);
                    let container_str = usage_data
                        .container
                        .and_then(|cid| ctx.registry.resolve(cid).map(|p| format!("in {}", p)))
                        .unwrap_or_default();
                    if container_str.is_empty() {
                        println!(
                            "  {}. {} ({})",
                            i + 1,
                            context_str.green(),
                            ref_kind_str.dimmed()
                        );
                    } else {
                        println!(
                            "  {}. {} ({}) {}",
                            i + 1,
                            context_str.green(),
                            ref_kind_str.dimmed(),
                            container_str.dimmed()
                        );
                    }
                }
            }
            if usages.len() > 20 {
                println!("  {} {} more...", "...and".dimmed(), usages.len() - 20);
            }
        }
        TypeAnalysisMode::Impact => {
            println!("{}", "Mode: Impact".dimmed());
            let impact = typeflow.impact(symbol_id);

            println!("  {}", "Direct usages:".cyan());
            println!("    Count: {}", impact.direct_usages.len());

            println!("  {}", "Trait bound usages:".cyan());
            println!("    Count: {}", impact.bound_usages.len());

            println!("  {}", "Containing types:".cyan());
            for container_id in impact.containing_types.iter().take(10) {
                if let Some(path) = ctx.registry.resolve(*container_id) {
                    // SymbolRef format
                    println!("    - {}", format!("{:?}@{}", container_id, path).green());
                }
            }
            if impact.containing_types.len() > 10 {
                println!(
                    "    {} {} more...",
                    "...and".dimmed(),
                    impact.containing_types.len() - 10
                );
            }
        }
    }

    // Show supertraits (for traits)
    if let Some(trait_detail) = ctx.detail_store.trait_(symbol_id) {
        if !trait_detail.supertraits.is_empty() {
            println!("  {}", "Supertraits:".cyan());
            for st in &trait_detail.supertraits {
                println!("    - {}", st.green());
            }
        }
    }

    // Show implementors (for traits)
    let implementors: Vec<_> = ctx.code_graph.implementors_of(symbol_id).take(20).collect();
    if !implementors.is_empty() {
        println!("  {} ({}):", "Implementors".cyan(), implementors.len());
        for (i, impl_id) in implementors.iter().take(10).enumerate() {
            if let Some(path) = ctx.registry.resolve(*impl_id) {
                println!(
                    "    {}. {} {}",
                    i + 1,
                    path.to_string().green(),
                    format!("[{:?}]", impl_id).dimmed()
                );
            }
        }
        if implementors.len() > 10 {
            println!(
                "    {} {} more...",
                "...and".dimmed(),
                implementors.len() - 10
            );
        }
    }

    Ok(())
}

fn execute_flow_analysis(
    path: &PathBuf,
    name: Option<&str>,
    id: Option<&str>,
    mode: FlowAnalysisMode,
    _depth: usize,
) -> Result<()> {
    let graph = build_graph(path)?;
    let ctx = graph.context();

    let dataflow = &ctx.dataflow_graph;

    // Helper to format variable info with SymbolId and parent function
    let registry = &ctx.registry;
    let format_var = |var_id: VarId| -> String {
        let var_name = dataflow.var_name(var_id).unwrap_or("unknown");
        if let Some(data) = dataflow.var(var_id) {
            // Get parent function name
            let parent_info = registry
                .resolve(data.parent)
                .map(|p| p.name().to_string())
                .unwrap_or_else(|| format!("{:?}", data.parent));

            // Get var's SymbolPath (via VarSymbolMapping)
            let var_symbol = dataflow.var_to_symbol(var_id);
            let var_path = var_symbol.and_then(|sym| registry.resolve(sym));

            if let Some(path) = var_path {
                format!(
                    "{} ({:?}, in {}) [{}]",
                    var_name, data.kind, parent_info, path
                )
            } else {
                format!("{} ({:?}, in {})", var_name, data.kind, parent_info)
            }
        } else {
            format!("var_{:?}", var_id)
        }
    };

    let symbol_id = id
        .map(|id_str| {
            SymbolId::parse(id_str)
                .ok_or_else(|| anyhow::anyhow!("Invalid SymbolId format: '{}'", id_str))
        })
        .transpose()?;
    let (matching_vars, display_name) = dataflow
        .resolve_vars(symbol_id, name, ctx.registry())
        .ok_or_else(|| anyhow::anyhow!("Either name or --id must be provided"))?;

    println!(
        "{} '{}'",
        "Data Flow Analysis".cyan().bold(),
        display_name.yellow()
    );
    println!();

    if matching_vars.is_empty() {
        println!(
            "{}",
            format!(
                "'{}' not found. No variables tracked in dataflow graph for this symbol",
                display_name
            )
            .red()
        );
        return Ok(());
    }

    println!("  {} {}", "Found:".cyan(), matching_vars.len());
    for (i, &var_id) in matching_vars.iter().take(5).enumerate() {
        println!("    {}. {}", i + 1, format_var(var_id).green());
    }
    println!();

    match mode {
        FlowAnalysisMode::Provenance => {
            println!("  {}", "Provenance (where values come from):".cyan());
            for &var_id in matching_vars.iter().take(3) {
                let provenance = dataflow.provenance(var_id);
                println!(
                    "    {} -> {} sources",
                    format_var(var_id).yellow(),
                    provenance.len()
                );
                for (i, prov_id) in provenance.iter().take(5).enumerate() {
                    println!("      {}. {}", i + 1, format_var(*prov_id));
                }
            }
        }
        FlowAnalysisMode::Impact => {
            println!("  {}", "Impact (where values flow to):".cyan());
            for &var_id in matching_vars.iter().take(3) {
                let impact = dataflow.impact(var_id);
                println!(
                    "    {} -> {} targets",
                    format_var(var_id).yellow(),
                    impact.len()
                );
                for (i, impact_id) in impact.iter().take(5).enumerate() {
                    println!("      {}. {}", i + 1, format_var(*impact_id));
                }
            }
        }
        FlowAnalysisMode::Chain => {
            println!("  {}", "Chain (provenance + impact):".cyan());
            for &var_id in matching_vars.iter().take(3) {
                let provenance = dataflow.provenance(var_id);
                let impact = dataflow.impact(var_id);
                println!(
                    "    {} sources -> {} -> {} targets",
                    provenance.len(),
                    format_var(var_id).yellow(),
                    impact.len()
                );
            }
        }
        FlowAnalysisMode::Sources => {
            println!(
                "  {}",
                "Sources (data origins with no incoming flows):".cyan()
            );
            let sources: Vec<_> = dataflow
                .iter_vars()
                .filter(|(var_id, _)| dataflow.incoming(*var_id).is_empty())
                .take(20)
                .collect();
            for (i, (var_id, _)) in sources.iter().enumerate() {
                println!("    {}. {}", i + 1, format_var(*var_id));
            }
        }
        FlowAnalysisMode::Sinks => {
            println!(
                "  {}",
                "Sinks (data destinations with no outgoing flows):".cyan()
            );
            let sinks: Vec<_> = dataflow
                .iter_vars()
                .filter(|(var_id, _)| dataflow.outgoing(*var_id).is_empty())
                .take(20)
                .collect();
            for (i, (var_id, _)) in sinks.iter().enumerate() {
                println!("    {}. {}", i + 1, format_var(*var_id));
            }
        }
    }

    Ok(())
}

fn execute_borrow_analysis(
    path: &PathBuf,
    name: Option<&str>,
    id: Option<&str>,
    conflicts_only: bool,
) -> Result<()> {
    let graph = build_graph(path)?;
    let ctx = graph.context();

    let dataflow = &ctx.dataflow_graph;
    let tracker = dataflow.borrow_tracker();

    let symbol_id = id
        .map(|id_str| {
            SymbolId::parse(id_str)
                .ok_or_else(|| anyhow::anyhow!("Invalid SymbolId format: '{}'", id_str))
        })
        .transpose()?;
    let (matching_vars, display_name) = dataflow
        .resolve_vars(symbol_id, name, ctx.registry())
        .ok_or_else(|| anyhow::anyhow!("Either name or --id must be provided"))?;

    println!(
        "{} '{}'{}",
        "Borrow Analysis".cyan().bold(),
        display_name.yellow(),
        if conflicts_only {
            " (conflicts only)"
        } else {
            ""
        }
    );
    println!();

    if matching_vars.is_empty() {
        println!(
            "{}",
            format!(
                "'{}' not found. No variables tracked in dataflow graph for this symbol",
                display_name
            )
            .red()
        );
        return Ok(());
    }

    println!("  {} {}", "Found:".cyan(), matching_vars.len());

    let registry = &ctx.registry;
    for &var_id in matching_vars.iter().take(10) {
        let var_data = dataflow.var(var_id);
        let var_name = dataflow.var_name(var_id).unwrap_or("unknown").to_string();
        let line = var_data.map(|d| d.line).unwrap_or(0);

        // Get parent function context
        let parent_info = var_data
            .and_then(|d| {
                registry
                    .resolve(d.parent)
                    .map(|p| format!("in {}({:?})", p.name(), d.parent))
            })
            .unwrap_or_default();

        // Query for borrow conflicts directly from tracker
        let mut_conflicts = tracker.conflicts(var_id, BorrowKind::Mutable, line);
        let shared_conflicts = tracker.conflicts(var_id, BorrowKind::Shared, line);
        let has_conflict = !mut_conflicts.is_empty() || !shared_conflicts.is_empty();

        let location = if line > 0 {
            format!("line {}, {}", line, parent_info)
        } else if !parent_info.is_empty() {
            parent_info.clone()
        } else {
            String::new()
        };

        if conflicts_only {
            if has_conflict {
                if location.is_empty() {
                    println!("  {} {}", "Conflict:".red(), var_name.yellow());
                } else {
                    println!(
                        "  {} {} ({})",
                        "Conflict:".red(),
                        var_name.yellow(),
                        location.dimmed()
                    );
                }
                for conflict in mut_conflicts.iter().chain(shared_conflicts.iter()) {
                    println!("    - `{}`: {}", var_name, conflict);
                }
            }
        } else {
            let status = if !has_conflict {
                "OK".green()
            } else {
                "Conflict".red()
            };
            if location.is_empty() {
                println!("  {} - {}", var_name.yellow(), status);
            } else {
                println!(
                    "  {} ({}) - {}",
                    var_name.yellow(),
                    location.dimmed(),
                    status
                );
            }

            for conflict in mut_conflicts.iter().chain(shared_conflicts.iter()) {
                println!("    - `{}`: {}", var_name, conflict);
            }
        }
    }

    Ok(())
}

fn execute_lock_analysis(
    path: &PathBuf,
    name: Option<&str>,
    id: Option<&str>,
    suggest: bool,
) -> Result<()> {
    let graph = build_graph(path)?;
    let ctx = graph.context();

    let dataflow = &ctx.dataflow_graph;
    let tracker = dataflow.lock_tracker();

    // --id mode: show lock acquisitions within the specified function
    // name mode: pattern-match lock variable names globally
    if let Some(id_str) = id {
        let symbol_id = SymbolId::parse(id_str)
            .ok_or_else(|| anyhow::anyhow!("Invalid SymbolId format: '{}'", id_str))?;
        let display_name = ctx
            .registry
            .resolve(symbol_id)
            .map(|p| p.name().to_string())
            .unwrap_or_else(|| id_str.to_string());

        println!(
            "{} '{}'{}",
            "Lock Analysis".cyan().bold(),
            display_name.yellow(),
            if suggest { " (with suggestions)" } else { "" }
        );
        println!();

        // Show locks owned by this function
        let owned = tracker.acquisitions_by_owner(symbol_id);
        println!(
            "  {} ({}):",
            "Lock acquisitions in function".cyan(),
            owned.len()
        );
        for acq in &owned {
            println!("    - {} ({:?})", acq.lock_name.green(), acq.lock_type);
        }
        if owned.is_empty() {
            println!("    {}", "No lock acquisitions in this function".dimmed());
        }
    } else {
        let display_name =
            name.ok_or_else(|| anyhow::anyhow!("Either name or --id must be provided"))?;

        println!(
            "{} '{}'{}",
            "Lock Analysis".cyan().bold(),
            display_name.yellow(),
            if suggest { " (with suggestions)" } else { "" }
        );
        println!();

        let analyzer = LockGranularityAnalyzerV2::new(tracker);

        // Show lock statistics
        let stats = analyzer.stats();
        println!("  {}", "Lock Statistics:".cyan());
        println!("    Total locks: {}", stats.total_locks);
        println!("    Mutex: {}", stats.mutex_count);
        println!("    RwLock: {}", stats.rwlock_count);
        println!("    RefCell: {}", stats.refcell_count);
        println!("    Field accesses: {}", stats.total_field_accesses);
        println!();

        // Show lock acquisitions matching the name (supports wildcards and lock-type names)
        println!(
            "  {} '{}':",
            "Lock acquisitions matching".cyan(),
            display_name
        );
        let acquisitions = tracker.acquisitions();
        let mut found = 0;
        for acq in acquisitions {
            if matches_lock_pattern(acq, display_name) {
                found += 1;
                println!("    - {} ({:?})", acq.lock_name.green(), acq.lock_type);
                if found >= 10 {
                    break;
                }
            }
        }
        if found == 0 {
            println!("    {}", "No locks found matching pattern".dimmed());
            let mut unique_names: Vec<&str> =
                acquisitions.iter().map(|a| a.lock_name.as_str()).collect();
            unique_names.sort();
            unique_names.dedup();
            if !unique_names.is_empty() {
                let preview: Vec<&str> = unique_names.iter().take(10).copied().collect();
                println!(
                    "    {} {}",
                    "Available lock variables:".dimmed(),
                    preview.join(", ")
                );
            }
        }

        // Show suggestions if requested
        if suggest {
            println!();
            let suggestions = analyzer.analyze();
            if suggestions.is_empty() {
                println!("  {} {}", "Suggestions:".cyan(), "None".dimmed());
            } else {
                println!("  {} ({})", "Suggestions:".cyan(), suggestions.len());
                for (i, suggestion) in suggestions.iter().take(10).enumerate() {
                    println!("    {}. {}", i + 1, suggestion.short_description());
                }
            }
        }
    }

    Ok(())
}

/// Match a lock acquisition against a search pattern.
///
/// Supports:
/// - Wildcard `*` matches all acquisitions
/// - `prefix*`, `*suffix`, `*contains*` glob patterns (case-insensitive)
/// - Lock type names: `Mutex`, `RwLock`, `RefCell`, `TokioMutex`, etc.
/// - Plain substring match on lock_name (case-insensitive)
fn matches_lock_pattern(acq: &ryo_analysis::LockAcquisitionV2, pattern: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return true;
    }

    let pattern_lower = pattern.to_lowercase();

    // Check lock type name match (e.g., "Mutex", "RwLock", "RefCell")
    let type_name = format!("{:?}", acq.lock_type).to_lowercase();
    if type_name == pattern_lower || type_name.contains(&pattern_lower) {
        return true;
    }

    let name_lower = acq.lock_name.to_lowercase();

    // Glob-style matching
    if pattern_lower.starts_with('*') && pattern_lower.ends_with('*') && pattern_lower.len() > 1 {
        let middle = &pattern_lower[1..pattern_lower.len() - 1];
        if middle.is_empty() {
            return true;
        }
        name_lower.contains(middle)
    } else if let Some(stripped) = pattern_lower.strip_prefix('*') {
        name_lower.ends_with(stripped)
    } else if pattern_lower.ends_with('*') {
        name_lower.starts_with(&pattern_lower[..pattern_lower.len() - 1])
    } else {
        // Substring match (case-insensitive)
        name_lower.contains(&pattern_lower)
    }
}

fn execute_cascade_analysis(path: &PathBuf, id: &str, _depth: usize) -> Result<()> {
    let graph = build_graph(path)?;
    let ctx = graph.context();

    let symbol_id = SymbolId::parse(id).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid SymbolId format: '{}'. Expected format: '165v1'",
            id
        )
    })?;
    let symbol_path = ctx.registry.resolve(symbol_id);
    let display_name = symbol_path
        .as_ref()
        .map(|p| p.name().to_string())
        .unwrap_or_else(|| id.to_string());
    // SymbolRef format
    let symbol_ref = symbol_path
        .map(|p| format!("{:?}@{}", symbol_id, p))
        .unwrap_or_else(|| format!("{:?}", symbol_id));

    println!(
        "{} '{}' ({})",
        "Cascade Analysis".cyan().bold(),
        display_name.yellow(),
        symbol_ref.dimmed()
    );
    println!();

    let symbol_kind = ctx.registry.kind(symbol_id);

    // Show callers that would be affected
    println!("  {}", "Affected callers:".cyan());
    let callers: Vec<_> = ctx.code_graph.callers_of(symbol_id).collect();
    println!("    Count: {}", callers.len());
    for (i, caller_id) in callers.iter().take(10).enumerate() {
        if let Some(path) = ctx.registry.resolve(*caller_id) {
            println!("    {}. {:?}@{}", i + 1, caller_id, path);
        }
    }

    // Show types that use this symbol
    println!("  {}", "Types using this symbol:".cyan());
    let users: Vec<_> = ctx.typeflow_graph.type_users(symbol_id).collect();
    println!("    Count: {}", users.len());
    for (i, user_id) in users.iter().take(10).enumerate() {
        if let Some(path) = ctx.registry.resolve(*user_id) {
            println!("    {}. {:?}@{}", i + 1, user_id, path);
        }
    }

    // For enums: use Api layer which scans PureFiles for match functions
    if symbol_kind == Some(ryo_analysis::SymbolKind::Enum) {
        let api = ryo_app::Api::from_path(path)?;
        let cascade = api.graph_cascade(ryo_app::CascadeRequest::new(id))?;
        if !cascade.match_functions.is_empty() {
            println!("  {}", "Match functions (enum-specific):".cyan());
            println!("    Count: {}", cascade.match_functions.len());
            for (i, mf) in cascade.match_functions.iter().take(10).enumerate() {
                println!("    {}. {}", i + 1, mf);
            }
            if cascade.match_functions.len() > 10 {
                println!("    ... and {} more", cascade.match_functions.len() - 10);
            }
        }
    }

    // Show type impact (containing types)
    let impact = ctx.typeflow_graph.impact(symbol_id);
    if !impact.containing_types.is_empty() {
        println!("  {}", "Containing types:".cyan());
        println!("    Count: {}", impact.containing_types.len());
        for (i, container_id) in impact.containing_types.iter().take(10).enumerate() {
            if let Some(path) = ctx.registry.resolve(*container_id) {
                println!("    {}. {:?}@{}", i + 1, container_id, path);
            }
        }
    }

    // Show direct usages from typeflow
    if !impact.direct_usages.is_empty() {
        println!("  {}", "Direct type usages:".cyan());
        println!("    Count: {}", impact.direct_usages.len());
    }

    Ok(())
}

// =============================================================================
// Server mode execution
// =============================================================================

/// Execute cascade via server with connect-or-spawn
fn execute_cascade_via_server_spawn(
    project: &std::path::Path,
    id: &str,
    depth: usize,
) -> Result<()> {
    use super::ExecutionMode;
    use crate::client::{connect_or_spawn, slow_context};
    use ryo_app::CascadeRequest;

    let start = std::time::Instant::now();
    ExecutionMode::Client.print_banner();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = connect_or_spawn(Some(project)).await?;

        let request = CascadeRequest {
            id: id.to_string(),
            uuid: None,
            depth: Some(depth),
        };

        let response = client
            .cascade(slow_context(), request)
            .await
            .map_err(|e| anyhow::anyhow!("RPC call failed: {}", e))?
            .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

        let elapsed = start.elapsed();

        println!(
            "{} '{}' ({:?})",
            "Cascade Analysis".cyan().bold(),
            response.display_name.yellow(),
            elapsed
        );
        println!();

        println!("  {}", "Affected callers:".cyan());
        println!("    Count: {}", response.callers.len());
        for (i, caller) in response.callers.iter().take(10).enumerate() {
            println!("    {}. {}", i + 1, caller);
        }
        if response.callers.len() > 10 {
            println!("    ... and {} more", response.callers.len() - 10);
        }

        println!("  {}", "Types using this symbol:".cyan());
        println!("    Count: {}", response.users.len());
        for (i, user) in response.users.iter().take(10).enumerate() {
            println!("    {}. {}", i + 1, user);
        }
        if response.users.len() > 10 {
            println!("    ... and {} more", response.users.len() - 10);
        }

        if !response.match_functions.is_empty() {
            println!("  {}", "Match functions (enum-specific):".cyan());
            println!("    Count: {}", response.match_functions.len());
            for (i, mf) in response.match_functions.iter().take(10).enumerate() {
                println!("    {}. {}", i + 1, mf);
            }
            if response.match_functions.len() > 10 {
                println!("    ... and {} more", response.match_functions.len() - 10);
            }
        }

        if !response.containing_types.is_empty() {
            println!("  {}", "Containing types:".cyan());
            println!("    Count: {}", response.containing_types.len());
            for (i, ct) in response.containing_types.iter().take(10).enumerate() {
                println!("    {}. {}", i + 1, ct);
            }
            if response.containing_types.len() > 10 {
                println!("    ... and {} more", response.containing_types.len() - 10);
            }
        }

        Ok::<_, anyhow::Error>(())
    })
}

/// Execute summary via server with connect-or-spawn
fn execute_summary_via_server_spawn(
    project: &std::path::Path,
    detailed: bool,
    compact: bool,
    max_items: Option<usize>,
    output: Option<PathBuf>,
) -> Result<()> {
    use super::ExecutionMode;
    use crate::client::{connect_or_spawn, slow_context};
    use ryo_app::GraphSummaryRequest;

    let start = std::time::Instant::now();
    ExecutionMode::Client.print_banner();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = connect_or_spawn(Some(project)).await?;

        let request = GraphSummaryRequest {
            detailed,
            compact,
            max_items,
        };

        let response = client
            .graph_summary(slow_context(), request)
            .await
            .map_err(|e| anyhow::anyhow!("RPC call failed: {}", e))?
            .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

        let elapsed = start.elapsed();

        if let Some(output_path) = output {
            std::fs::write(&output_path, &response.content)?;
            println!(
                "{} {} ({:?})",
                "Written to:".green(),
                output_path.display(),
                elapsed
            );
        } else {
            println!("{} ({:?})", "CodeGraph Summary".cyan().bold(), elapsed);
            println!(
                "  {} nodes, {} edges, {} files",
                response.node_count.to_string().yellow(),
                response.edge_count.to_string().yellow(),
                response.file_count.to_string().yellow(),
            );
            println!();
            println!("{}", response.content);
        }

        Ok::<_, anyhow::Error>(())
    })
}

/// Execute type analysis via server with connect-or-spawn
fn execute_type_via_server_spawn(
    project: &std::path::Path,
    name: Option<&str>,
    id: Option<&str>,
    mode: TypeAnalysisMode,
    depth: usize,
) -> Result<()> {
    use super::ExecutionMode;
    use crate::client::{connect_or_spawn, slow_context};
    use ryo_app::api::{TypeAnalysisMode as ApiMode, TypeAnalysisRequest};

    let start = std::time::Instant::now();
    ExecutionMode::Client.print_banner();

    let api_mode = match mode {
        TypeAnalysisMode::Definition => ApiMode::Definition,
        TypeAnalysisMode::Usage => ApiMode::Usage,
        TypeAnalysisMode::Impact => ApiMode::Impact,
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = connect_or_spawn(Some(project)).await?;

        let request = TypeAnalysisRequest {
            name: name.map(|s| s.to_string()),
            id: id.map(|s| s.to_string()),
            uuid: None,
            mode: api_mode,
            depth: Some(depth),
        };

        let response = client
            .graph_type(slow_context(), request)
            .await
            .map_err(|e| anyhow::anyhow!("RPC call failed: {}", e))?
            .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

        let elapsed = start.elapsed();

        // SymbolRef format: combine symbol_id with mod_path
        let symbol_ref = response
            .mod_path
            .as_ref()
            .map(|p| format!("{}@{}", response.symbol_id, p))
            .unwrap_or_else(|| response.symbol_id.clone());

        println!(
            "{} '{}' ({}) ({:?})",
            "Type Analysis".cyan().bold(),
            response.display_name.yellow(),
            symbol_ref.dimmed(),
            elapsed
        );
        println!();

        if let Some(kind) = &response.kind {
            println!("  {} {}", "Kind:".cyan(), kind);
        }

        match mode {
            TypeAnalysisMode::Definition => {
                // Definition: show structure details
                if let Some(generics) = &response.generics {
                    println!("  {} {}", "Generics:".cyan(), generics);
                }
                if !response.attrs.is_empty() {
                    println!("  {} {}", "Attributes:".cyan(), response.attrs.join(", "));
                }
                if !response.fields.is_empty() {
                    println!("  {} ({})", "Fields:".cyan(), response.fields.len());
                    for f in &response.fields {
                        let vis = if f.is_public { "pub " } else { "" };
                        println!("    - {}{}: {}", vis, f.name.green(), f.ty);
                    }
                }
                if !response.variants.is_empty() {
                    println!("  {} ({})", "Variants:".cyan(), response.variants.len());
                    for v in &response.variants {
                        if v.fields.is_empty() {
                            println!("    - {}", v.name.green());
                        } else {
                            let fields: Vec<String> = v
                                .fields
                                .iter()
                                .map(|f| format!("{}: {}", f.name, f.ty))
                                .collect();
                            println!("    - {} {{ {} }}", v.name.green(), fields.join(", "));
                        }
                    }
                }
                if !response.params.is_empty() {
                    println!("  {}", "Parameters:".cyan());
                    for p in &response.params {
                        println!("    - {}: {}", p.name.green(), p.ty);
                    }
                }
                if let Some(ret) = &response.return_type {
                    println!("  {} {}", "Returns:".cyan(), ret);
                }
                if !response.methods.is_empty() {
                    println!("  {} ({})", "Methods:".cyan(), response.methods.len());
                    for m in &response.methods {
                        println!("    - {}", m.green());
                    }
                }
                if !response.supertraits.is_empty() {
                    println!("  {}", "Supertraits:".cyan());
                    for st in &response.supertraits {
                        println!("    - {}", st.green());
                    }
                }
                if !response.implementors.is_empty() {
                    println!(
                        "  {} ({}):",
                        "Implementors".cyan(),
                        response.implementors.len()
                    );
                    for (i, impl_name) in response.implementors.iter().take(10).enumerate() {
                        println!("    {}. {}", i + 1, impl_name);
                    }
                    if response.implementors.len() > 10 {
                        println!(
                            "    {} {} more...",
                            "...and".dimmed(),
                            response.implementors.len() - 10
                        );
                    }
                }
                println!(
                    "  {} {}",
                    "Usage count:".cyan().dimmed(),
                    response.usage_count
                );
            }
            TypeAnalysisMode::Usage => {
                // Usage: show usage list
                println!("  {} {}", "Usage count:".cyan(), response.usage_count);
                if !response.usages.is_empty() {
                    println!("  {}", "Usages:".cyan());
                    for (i, usage) in response.usages.iter().take(10).enumerate() {
                        if let Some(ref container) = usage.container {
                            println!(
                                "    {}. {} ({}) {}",
                                i + 1,
                                usage.context.green(),
                                usage.ref_kind.dimmed(),
                                format!("in {}", container).dimmed()
                            );
                        } else {
                            println!(
                                "    {}. {} ({})",
                                i + 1,
                                usage.context.green(),
                                usage.ref_kind.dimmed()
                            );
                        }
                    }
                    if response.usage_count > 10 {
                        println!(
                            "    {} {} more...",
                            "...and".dimmed(),
                            response.usage_count - 10
                        );
                    }
                }
            }
            TypeAnalysisMode::Impact => {
                // Impact: usage summary + impact details
                println!("  {} {}", "Usage count:".cyan(), response.usage_count);
                if !response.usages.is_empty() {
                    println!("  {}", "Usages:".cyan());
                    for (i, usage) in response.usages.iter().take(10).enumerate() {
                        if let Some(ref container) = usage.container {
                            println!(
                                "    {}. {} ({}) {}",
                                i + 1,
                                usage.context.green(),
                                usage.ref_kind.dimmed(),
                                format!("in {}", container).dimmed()
                            );
                        } else {
                            println!(
                                "    {}. {} ({})",
                                i + 1,
                                usage.context.green(),
                                usage.ref_kind.dimmed()
                            );
                        }
                    }
                }
                if let Some(impact) = &response.impact {
                    println!("  {}", "Impact:".cyan());
                    println!("    Direct usages: {}", impact.direct_usages);
                    println!("    Bound usages: {}", impact.bound_usages);
                    if !impact.containing_types.is_empty() {
                        println!("    Containing types:");
                        for ct in impact.containing_types.iter().take(5) {
                            println!("      - {}", ct);
                        }
                    }
                }
            }
        }

        Ok::<_, anyhow::Error>(())
    })
}

/// Execute flow analysis via server with connect-or-spawn
fn execute_flow_via_server_spawn(
    project: &std::path::Path,
    name: Option<&str>,
    id: Option<&str>,
    mode: FlowAnalysisMode,
    _depth: usize,
) -> Result<()> {
    use super::ExecutionMode;
    use crate::client::{connect_or_spawn, slow_context};
    use ryo_app::api::{FlowAnalysisMode as ApiMode, FlowAnalysisRequest};

    let start = std::time::Instant::now();
    ExecutionMode::Client.print_banner();

    let api_mode = match mode {
        FlowAnalysisMode::Provenance => ApiMode::Provenance,
        FlowAnalysisMode::Impact => ApiMode::Impact,
        FlowAnalysisMode::Chain => ApiMode::Chain,
        FlowAnalysisMode::Sources => ApiMode::Sources,
        FlowAnalysisMode::Sinks => ApiMode::Sinks,
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = connect_or_spawn(Some(project)).await?;

        let request = FlowAnalysisRequest {
            name: name.map(|s| s.to_string()),
            id: id.map(|s| s.to_string()),
            uuid: None,
            mode: api_mode,
            depth: None,
        };

        let response = client
            .graph_flow(slow_context(), request)
            .await
            .map_err(|e| anyhow::anyhow!("RPC call failed: {}", e))?
            .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

        let elapsed = start.elapsed();

        println!(
            "{} '{}' ({:?})",
            "Data Flow Analysis".cyan().bold(),
            response.display_name.yellow(),
            elapsed
        );
        println!();

        println!("  {} {}", "Found:".cyan(), response.found_vars.len());
        for (i, var) in response.found_vars.iter().take(5).enumerate() {
            if let Some(ref sp) = var.symbol_path {
                println!(
                    "    {}. {} ({}, in {}) [{}]",
                    i + 1,
                    var.name.green(),
                    var.kind,
                    var.parent,
                    sp
                );
            } else {
                println!(
                    "    {}. {} ({}, in {})",
                    i + 1,
                    var.name.green(),
                    var.kind,
                    var.parent
                );
            }
        }

        if !response.provenance.is_empty() {
            println!();
            println!("  {}", "Provenance (sources):".cyan());
            for (i, var) in response.provenance.iter().take(5).enumerate() {
                println!("    {}. {} ({})", i + 1, var.name, var.parent);
            }
        }

        if !response.impact.is_empty() {
            println!();
            println!("  {}", "Impact (targets):".cyan());
            for (i, var) in response.impact.iter().take(5).enumerate() {
                println!("    {}. {} ({})", i + 1, var.name, var.parent);
            }
        }

        if !response.sources.is_empty() {
            println!();
            println!("  {}", "Sources (data origins):".cyan());
            for (i, var) in response.sources.iter().take(10).enumerate() {
                println!("    {}. {} ({})", i + 1, var.name, var.parent);
            }
        }

        if !response.sinks.is_empty() {
            println!();
            println!("  {}", "Sinks (data destinations):".cyan());
            for (i, var) in response.sinks.iter().take(10).enumerate() {
                println!("    {}. {} ({})", i + 1, var.name, var.parent);
            }
        }

        Ok::<_, anyhow::Error>(())
    })
}

/// Execute borrow analysis via server with connect-or-spawn
fn execute_borrow_via_server_spawn(
    project: &std::path::Path,
    name: Option<&str>,
    id: Option<&str>,
    conflicts_only: bool,
) -> Result<()> {
    use super::ExecutionMode;
    use crate::client::{connect_or_spawn, slow_context};
    use ryo_app::api::BorrowAnalysisRequest;

    let start = std::time::Instant::now();
    ExecutionMode::Client.print_banner();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = connect_or_spawn(Some(project)).await?;

        let request = BorrowAnalysisRequest {
            name: name.map(|s| s.to_string()),
            id: id.map(|s| s.to_string()),
            uuid: None,
            conflicts_only,
        };

        let response = client
            .graph_borrow(slow_context(), request)
            .await
            .map_err(|e| anyhow::anyhow!("RPC call failed: {}", e))?
            .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

        let elapsed = start.elapsed();

        println!(
            "{} '{}'{}({:?})",
            "Borrow Analysis".cyan().bold(),
            response.display_name.yellow(),
            if conflicts_only {
                " (conflicts only) "
            } else {
                " "
            },
            elapsed
        );
        println!();

        println!("  {} {}", "Found:".cyan(), response.found_count);

        for status in &response.statuses {
            let status_str = if status.has_conflict {
                "Conflict".red()
            } else {
                "OK".green()
            };
            let location = if status.line > 0 {
                format!("line {}, {}", status.line, status.parent_info)
            } else if !status.parent_info.is_empty() {
                status.parent_info.clone()
            } else {
                String::new()
            };
            if location.is_empty() {
                println!("  {} - {}", status.var_name.yellow(), status_str);
            } else {
                println!(
                    "  {} ({}) - {}",
                    status.var_name.yellow(),
                    location.dimmed(),
                    status_str
                );
            }
            for err in &status.errors {
                println!("    - {}", err);
            }
        }

        Ok::<_, anyhow::Error>(())
    })
}

/// Execute lock analysis via server with connect-or-spawn
fn execute_lock_via_server_spawn(
    project: &std::path::Path,
    name: Option<&str>,
    id: Option<&str>,
    suggest: bool,
) -> Result<()> {
    use super::ExecutionMode;
    use crate::client::{connect_or_spawn, slow_context};
    use ryo_app::api::LockAnalysisRequest;

    let start = std::time::Instant::now();
    ExecutionMode::Client.print_banner();

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = connect_or_spawn(Some(project)).await?;

        let request = LockAnalysisRequest {
            name: name.map(|s| s.to_string()),
            id: id.map(|s| s.to_string()),
            uuid: None,
            suggest,
        };

        let response = client
            .graph_lock(slow_context(), request)
            .await
            .map_err(|e| anyhow::anyhow!("RPC call failed: {}", e))?
            .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

        let elapsed = start.elapsed();

        println!(
            "{} '{}'{}({:?})",
            "Lock Analysis".cyan().bold(),
            response.display_name.yellow(),
            if suggest { " (with suggestions) " } else { " " },
            elapsed
        );
        println!();

        println!("  {}", "Lock Statistics:".cyan());
        println!("    Total locks: {}", response.stats.total_locks);
        println!("    Mutex: {}", response.stats.mutex_count);
        println!("    RwLock: {}", response.stats.rwlock_count);
        println!("    RefCell: {}", response.stats.refcell_count);
        println!(
            "    Field accesses: {}",
            response.stats.total_field_accesses
        );
        println!();

        if id.is_some() {
            // --id mode: show locks owned by the function
            println!(
                "  {} ({}):",
                "Lock acquisitions in function".cyan(),
                response.acquisitions.len()
            );
            if !response.acquisitions.is_empty() {
                for acq in &response.acquisitions {
                    println!("    - {} ({})", acq.lock_name.green(), acq.lock_type);
                }
            } else {
                println!("    {}", "No lock acquisitions in this function".dimmed());
            }
        } else {
            // name mode: show locks matching the pattern
            println!(
                "  {} '{}':",
                "Lock acquisitions matching".cyan(),
                response.display_name
            );
            if !response.acquisitions.is_empty() {
                for acq in &response.acquisitions {
                    println!("    - {} ({})", acq.lock_name.green(), acq.lock_type);
                }
            } else {
                println!("    {}", "No locks found matching pattern".dimmed());
            }
        }

        if suggest && !response.suggestions.is_empty() {
            println!();
            println!(
                "  {} ({}):",
                "Suggestions".cyan(),
                response.suggestions.len()
            );
            for (i, s) in response.suggestions.iter().enumerate() {
                println!(
                    "    {}. {} '{}': {}",
                    i + 1,
                    s.kind,
                    s.target.yellow(),
                    s.description,
                );
            }
        }

        Ok::<_, anyhow::Error>(())
    })
}

// =============================================================================
// Chain Analysis (Transitive Call Chain)
// =============================================================================

/// Execute chain analysis in standalone mode
fn execute_chain_analysis(
    path: &PathBuf,
    id: &str,
    mode: ChainMode,
    max_depth: usize,
) -> Result<()> {
    use ryo_analysis::ChainDirection;

    let graph = build_graph(path)?;
    let ctx = graph.context();

    let symbol_id = SymbolId::parse(id).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid SymbolId format: '{}'. Expected format: '165v1'",
            id
        )
    })?;

    let symbol_path = ctx.registry.resolve(symbol_id);
    let display_name = symbol_path
        .as_ref()
        .map(|p| p.name().to_string())
        .unwrap_or_else(|| id.to_string());
    // SymbolRef format
    let symbol_ref = symbol_path
        .map(|p| format!("{:?}@{}", symbol_id, p))
        .unwrap_or_else(|| format!("{:?}", symbol_id));

    // Execute chain analysis — Callers/Callees via CodeGraph, TypeUsers/TypeDeps via TypeFlow
    let result = match mode {
        ChainMode::Callers => {
            ctx.code_graph
                .analyze_chain(symbol_id, max_depth, ChainDirection::Callers)
        }
        ChainMode::Callees => {
            ctx.code_graph
                .analyze_chain(symbol_id, max_depth, ChainDirection::Callees)
        }
        ChainMode::TypeUsers => {
            ctx.typeflow_graph
                .analyze_type_chain(symbol_id, max_depth, ChainDirection::TypeUsers)
        }
        ChainMode::TypeDeps => {
            ctx.typeflow_graph
                .analyze_type_chain(symbol_id, max_depth, ChainDirection::TypeDeps)
        }
    };

    println!(
        "{} '{}' ({})",
        "Chain Analysis".cyan().bold(),
        display_name.yellow(),
        symbol_ref.dimmed()
    );
    println!();

    println!(
        "  {} {} (depth: {}/{})",
        format!("{}:", result.direction).cyan(),
        result.total_count(),
        result.max_actual_depth,
        max_depth
    );
    println!();

    if result.is_empty() {
        println!("  {}", "No symbols found in chain".dimmed());
        return Ok(());
    }

    // Group by depth for display
    for depth in 1..=result.max_actual_depth {
        let nodes_at_depth: Vec<_> = result.at_depth(depth).collect();
        if nodes_at_depth.is_empty() {
            continue;
        }

        println!(
            "  {} ({})",
            format!("Depth {}:", depth).cyan(),
            nodes_at_depth.len()
        );
        for (i, node) in nodes_at_depth.iter().take(10).enumerate() {
            if let Some(path) = ctx.registry.resolve(node.symbol) {
                let kind = ctx
                    .registry
                    .kind(node.symbol)
                    .map(|k| format!("{:?}", k))
                    .unwrap_or_else(|| "?".to_string());
                // SymbolRef format
                println!(
                    "    {}. {:?}@{} ({})",
                    i + 1,
                    node.symbol,
                    path,
                    kind.dimmed()
                );
            }
        }
        if nodes_at_depth.len() > 10 {
            println!(
                "    {} and {} more...",
                "...".dimmed(),
                nodes_at_depth.len() - 10
            );
        }
    }

    // Summary
    println!();
    println!(
        "  {}: {} symbols across {} depth levels",
        "Total".cyan(),
        result.total_count(),
        result.max_actual_depth
    );

    Ok(())
}

/// Execute chain analysis via server with connect-or-spawn
fn execute_chain_via_server_spawn(
    project: &std::path::Path,
    id: &str,
    mode: ChainMode,
    depth: usize,
) -> Result<()> {
    use super::ExecutionMode;
    use crate::client::{connect_or_spawn, slow_context};
    use ryo_app::api::{ChainAnalysisRequest, ChainMode as ApiChainMode};

    let start = std::time::Instant::now();
    ExecutionMode::Client.print_banner();

    let api_mode = match mode {
        ChainMode::Callers => ApiChainMode::Callers,
        ChainMode::Callees => ApiChainMode::Callees,
        ChainMode::TypeUsers => ApiChainMode::TypeUsers,
        ChainMode::TypeDeps => ApiChainMode::TypeDeps,
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = connect_or_spawn(Some(project)).await?;

        let request = ChainAnalysisRequest {
            id: id.to_string(),
            uuid: None,
            mode: api_mode,
            depth: Some(depth),
        };

        let response = client
            .graph_chain(slow_context(), request)
            .await
            .map_err(|e| anyhow::anyhow!("RPC call failed: {}", e))?
            .map_err(|e| anyhow::anyhow!("Server error: {}", e))?;

        let elapsed = start.elapsed();

        println!(
            "{} '{}' ({:?})",
            "Chain Analysis".cyan().bold(),
            response.display_name.yellow(),
            elapsed
        );
        println!();

        println!(
            "  {} {} (depth: {}/{})",
            format!("{}:", response.direction).cyan(),
            response.total_count,
            response.max_actual_depth,
            depth
        );
        println!();

        if response.total_count == 0 {
            println!("  {}", "No symbols found in chain".dimmed());
            return Ok::<_, anyhow::Error>(());
        }

        // Display by depth
        for (depth_level, nodes) in &response.by_depth {
            println!(
                "  {} ({}):",
                format!("Depth {}:", depth_level).cyan(),
                nodes.len()
            );
            for (i, node) in nodes.iter().take(10).enumerate() {
                println!(
                    "    {}. {} ({}) [{}]",
                    i + 1,
                    node.path,
                    node.kind.as_deref().unwrap_or("?").to_string().dimmed(),
                    node.id
                );
            }
            if nodes.len() > 10 {
                println!("    {} and {} more...", "...".dimmed(), nodes.len() - 10);
            }
        }

        println!();
        println!(
            "  {}: {} symbols across {} depth levels",
            "Total".cyan(),
            response.total_count,
            response.max_actual_depth
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[cfg(test)]
mod tests {
    use super::matches_lock_pattern;
    use ryo_analysis::{LockAcquisitionV2, LockType, VarId, VarSymbolMapping};
    use slotmap::SlotMap;

    struct TestVars {
        symbols: SlotMap<ryo_analysis::SymbolId, &'static str>,
        mapping: VarSymbolMapping,
    }

    impl TestVars {
        fn new() -> Self {
            Self {
                symbols: SlotMap::with_key(),
                mapping: VarSymbolMapping::new(),
            }
        }

        fn var(&mut self, name: &'static str) -> VarId {
            let sym = self.symbols.insert(name);
            self.mapping.register(sym)
        }
    }

    fn make_acq(lock_name: &str, lock_type: LockType) -> LockAcquisitionV2 {
        let mut vars = TestVars::new();
        let lock = vars.var("lock");
        let guard = vars.var("guard");
        LockAcquisitionV2::new(lock, guard, lock_type, 0, lock_name, "g")
    }

    #[test]
    fn wildcard_star_matches_all() {
        let acq = make_acq("inner", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, "*"));
    }

    #[test]
    fn empty_pattern_matches_all() {
        let acq = make_acq("inner", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, ""));
    }

    #[test]
    fn exact_name_match() {
        let acq = make_acq("inner", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, "inner"));
        assert!(!matches_lock_pattern(&acq, "outer"));
    }

    #[test]
    fn substring_match() {
        let acq = make_acq("shared_state", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, "shared"));
        assert!(matches_lock_pattern(&acq, "state"));
    }

    #[test]
    fn glob_prefix() {
        let acq = make_acq("shared_state", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, "shared*"));
        assert!(!matches_lock_pattern(&acq, "state*"));
    }

    #[test]
    fn glob_suffix() {
        let acq = make_acq("shared_state", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, "*state"));
        assert!(!matches_lock_pattern(&acq, "*shared"));
    }

    #[test]
    fn glob_contains() {
        let acq = make_acq("my_shared_state", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, "*shared*"));
    }

    #[test]
    fn type_name_mutex() {
        let acq = make_acq("inner", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, "Mutex"));
        assert!(matches_lock_pattern(&acq, "mutex"));
    }

    #[test]
    fn type_name_rwlock() {
        let acq = make_acq("data", LockType::RwLockRead);
        assert!(matches_lock_pattern(&acq, "RwLock"));
        assert!(matches_lock_pattern(&acq, "rwlock"));
    }

    #[test]
    fn type_name_refcell() {
        let acq = make_acq("cache", LockType::RefCell);
        assert!(matches_lock_pattern(&acq, "RefCell"));
        assert!(matches_lock_pattern(&acq, "refcell"));
    }

    #[test]
    fn type_name_tokio_mutex() {
        let acq = make_acq("conn", LockType::TokioMutex);
        assert!(matches_lock_pattern(&acq, "TokioMutex"));
        assert!(matches_lock_pattern(&acq, "tokio"));
    }

    #[test]
    fn case_insensitive() {
        let acq = make_acq("SharedState", LockType::Mutex);
        assert!(matches_lock_pattern(&acq, "sharedstate"));
        assert!(matches_lock_pattern(&acq, "SHAREDSTATE"));
    }

    #[test]
    fn no_match() {
        let acq = make_acq("inner", LockType::Mutex);
        assert!(!matches_lock_pattern(&acq, "nonexistent"));
    }
}
