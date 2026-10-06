pub mod agents;
pub mod audit;
pub mod config;
pub mod domain;
pub mod llm;
pub mod mcp;
pub mod orchestrator;
pub mod planner;
pub mod project;
pub mod registry;
pub mod staffing;
pub mod workbench;
pub mod workspace;

/// Boxed future used by the object-safe `Agent` and `LlmProvider` traits.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
