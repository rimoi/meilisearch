/*! Module implementing the graph used for the graph-based ranking rules
and its related algorithms.

A ranking rule graph is built on top of the [`QueryGraph`]: the nodes stay
the same but the edges are replaced.
*/

mod build;
mod cheapest_paths;
mod condition_docids_cache;
mod dead_end_path_cache;
mod path_set;

/// Implementation of the `proximity` ranking rule
mod proximity;
/// Implementation of the `typo` ranking rule
mod typo;

use std::collections::HashSet;
use std::hash::Hash;

pub use condition_docids_cache::EdgeConditionDocIdsCache;
pub use dead_end_path_cache::DeadEndPathCache;
pub use proximity::{ProximityCondition, ProximityGraph};
use roaring::RoaringBitmap;
pub use typo::{TypoEdge, TypoGraph};

use super::interner::{DedupInterner, FixedSizeInterner, Interned, MappedInterner};
use super::logger::SearchLogger;
use super::query_term::Phrase;
use super::small_bitmap::SmallBitmap;
use super::{QueryGraph, QueryNode, SearchContext};
use crate::Result;

/// An edge in the ranking rule graph.
///
/// It contains:
/// 1. The source and destination nodes
/// 2. The cost of traversing this edge
/// 3. The condition associated with it
#[derive(Clone)]
pub struct Edge<E> {
    pub source_node: Interned<QueryNode>,
    pub dest_node: Interned<QueryNode>,
    pub cost: u8,
    pub condition: Option<Interned<E>>,
}

impl<E> Hash for Edge<E> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.source_node.hash(state);
        self.dest_node.hash(state);
        self.cost.hash(state);
        self.condition.hash(state);
    }
}

impl<E> Eq for Edge<E> {}

impl<E> PartialEq for Edge<E> {
    fn eq(&self, other: &Self) -> bool {
        self.source_node == other.source_node
            && self.dest_node == other.dest_node
            && self.cost == other.cost
            && self.condition == other.condition
    }
}

/// A trait to be implemented by a marker type to build a graph-based ranking rule.
///
/// It mostly describes how to:
/// 1. Retrieve the set of edges (their cost and condition) between two nodes.
/// 2. Compute the document ids satisfying a condition
pub trait RankingRuleGraphTrait: Sized {
    /// The condition of an edge connecting two query nodes. The condition
    /// should be sufficient to compute the edge's cost and associated document ids
    /// in [`resolve_edge_condition`](RankingRuleGraphTrait::resolve_edge_condition).
    type EdgeCondition: Sized + Clone + PartialEq + Eq + Hash;

    /// Return the label of the given edge condition, to be used when visualising
    /// the ranking rule graph.
    fn label_for_edge_condition<'ctx>(
        ctx: &mut SearchContext<'ctx>,
        edge: &Self::EdgeCondition,
    ) -> Result<String>;

    fn words_used_by_edge_condition<'ctx>(
        ctx: &mut SearchContext<'ctx>,
        edge: &Self::EdgeCondition,
    ) -> Result<HashSet<Interned<String>>>;
    fn phrases_used_by_edge_condition<'ctx>(
        ctx: &mut SearchContext<'ctx>,
        edge: &Self::EdgeCondition,
    ) -> Result<HashSet<Interned<Phrase>>>;

    /// Compute the document ids associated with the given edge condition,
    /// restricted to the given universe.
    fn resolve_edge_condition<'ctx>(
        ctx: &mut SearchContext<'ctx>,
        edge_condition: &Self::EdgeCondition,
        universe: &RoaringBitmap,
    ) -> Result<RoaringBitmap>;

    /// Return the cost and condition of the edges going from the previously visited node
    /// (with [`build_step_visit_source_node`](RankingRuleGraphTrait::build_step_visit_source_node)) to `dest_node`.
    fn build_edges<'ctx>(
        ctx: &mut SearchContext<'ctx>,
        conditions_interner: &mut DedupInterner<Self::EdgeCondition>,
        source_node: &QueryNode,
        dest_node: &QueryNode,
    ) -> Result<Vec<(u8, Option<Interned<Self::EdgeCondition>>)>>;

    fn log_state(
        graph: &RankingRuleGraph<Self>,
        paths: &[Vec<Interned<Self::EdgeCondition>>],
        dead_end_path_cache: &DeadEndPathCache<Self>,
        universe: &RoaringBitmap,
        distances: &MappedInterner<Vec<(u16, SmallBitmap<Self::EdgeCondition>)>, QueryNode>,
        cost: u16,
        logger: &mut dyn SearchLogger<QueryGraph>,
    );
}

/// The graph used by graph-based ranking rules.
///
/// It is built on top of a [`QueryGraph`], keeping the same nodes
/// but replacing the edges.
pub struct RankingRuleGraph<G: RankingRuleGraphTrait> {
    pub query_graph: QueryGraph,
    pub edges_store: FixedSizeInterner<Option<Edge<G::EdgeCondition>>>,
    pub edges_of_node: MappedInterner<SmallBitmap<Option<Edge<G::EdgeCondition>>>, QueryNode>,
    pub conditions_interner: FixedSizeInterner<G::EdgeCondition>,
}
impl<G: RankingRuleGraphTrait> Clone for RankingRuleGraph<G> {
    fn clone(&self) -> Self {
        Self {
            query_graph: self.query_graph.clone(),
            edges_store: self.edges_store.clone(),
            edges_of_node: self.edges_of_node.clone(),
            conditions_interner: self.conditions_interner.clone(),
        }
    }
}
impl<G: RankingRuleGraphTrait> RankingRuleGraph<G> {
    /// Remove all edges with the given condition
    pub fn remove_edges_with_condition(&mut self, condition_to_remove: Interned<G::EdgeCondition>) {
        for (edge_id, edge_opt) in self.edges_store.iter_mut() {
            let Some(edge) = edge_opt.as_mut() else { continue };
            let Some(condition) = edge.condition else { continue };

            if condition == condition_to_remove {
                let (source_node, _dest_node) = (edge.source_node, edge.dest_node);
                *edge_opt = None;
                self.edges_of_node.get_mut(source_node).remove(edge_id);
            }
        }
    }
}