//! Incremental layered layout for the traversal graph
//!
//! Each weakly-connected component is laid out in layers: callers sit one
//! layer left of their callees, and nodes within a layer stack into slots.
//! Placement is append-only -- a placed node never moves within its component
//! (no jitter as the graph grows) -- except when an edge joins two components,
//! which relocates the smaller component's nodes into the larger one once.
//! Components pack vertically in discovery order, so growth only pushes later
//! components down.

use std::collections::{BTreeSet, HashMap};

use super::state::{GraphState, NodeId};

/// The horizontal world distance between adjacent layers
///
/// Wide enough for a ~16-character label plus a gap before the next layer.
const LAYER_SPACING: f64 = 18.0;

/// The vertical world distance between adjacent slots in a layer
const SLOT_SPACING: f64 = 2.0;

/// The vertical world gap between stacked components
const COMPONENT_GAP: f64 = 3.0;

/// A node's placement: its component plus its layer/slot grid cell
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Placement {
    /// The component index this node belongs to (a union-find leaf, not root)
    comp: usize,
    /// The horizontal layer within the component (callers left, callees right)
    layer: i32,
    /// The vertical slot within the layer, handed out append-only
    slot: u32,
}

/// The incremental layered layout of the traversal graph
#[derive(Debug, Default)]
pub struct Layout {
    /// Each placed node's component, layer, and slot
    pos: HashMap<NodeId, Placement>,
    /// The union-find parent of each component index
    parents: Vec<usize>,
    /// How many nodes each component root currently holds
    sizes: Vec<usize>,
    /// The next free slot per `(component root, layer)`
    next_slot: HashMap<(usize, i32), u32>,
    /// The edges already folded into the layout
    seen_edges: BTreeSet<(NodeId, NodeId)>,
}

impl Layout {
    /// Folds the graph's new nodes and edges into the layout
    ///
    /// Nodes are processed in discovery (`seq`) order and edges in their
    /// deterministic set order, so the same graph state always yields the
    /// same layout regardless of how many events landed between frames.
    ///
    /// # Arguments
    ///
    /// * `state` - The graph state to lay out
    /// * `extra_edges` - Enrichment edges to honor alongside the state's own
    pub fn update(&mut self, state: &GraphState, extra_edges: &BTreeSet<(NodeId, NodeId)>) {
        // place any new nodes in discovery order, attaching to a placed
        // neighbor when an edge already links them
        let mut fresh: Vec<_> = state
            .nodes
            .values()
            .filter(|node| !self.pos.contains_key(&node.id))
            .collect();
        fresh.sort_by_key(|node| node.seq);
        for node in fresh {
            // look for a placed neighbor over every known edge
            let attach = state
                .edges
                .iter()
                .chain(extra_edges.iter())
                .find_map(|(from, to)| {
                    if from == &node.id {
                        // this node calls `to`, so it sits one layer left of it
                        self.pos.get(to).map(|pos| (*pos, -1))
                    } else if to == &node.id {
                        // this node is called by `from`, so it sits one layer right
                        self.pos.get(from).map(|pos| (*pos, 1))
                    } else {
                        None
                    }
                });
            match attach {
                // attach next to the placed neighbor in the right direction
                Some((anchor, direction)) => {
                    let root = self.find(anchor.comp);
                    self.place(node.id.clone(), root, anchor.layer + direction);
                }
                // no placed neighbor: the node roots a brand new component
                None => {
                    let root = self.new_component();
                    self.place(node.id.clone(), root, 0);
                }
            }
        }
        // fold in any edges not seen yet, unioning components they join
        for edge in state.edges.iter().chain(extra_edges.iter()) {
            if self.seen_edges.contains(edge) {
                continue;
            }
            self.seen_edges.insert(edge.clone());
            // both endpoints are placed by now (edges only touch known nodes)
            let (Some(from), Some(to)) = (
                self.pos.get(&edge.0).copied(),
                self.pos.get(&edge.1).copied(),
            ) else {
                continue;
            };
            let from_root = self.find(from.comp);
            let to_root = self.find(to.comp);
            // an edge within one component changes nothing placed
            if from_root == to_root {
                continue;
            }
            // the edge joins two components: relocate the smaller into the larger
            self.merge_components(from_root, to_root, &from, &to);
        }
    }

    /// Computes every placed node's world coordinates
    ///
    /// Components stack vertically in discovery order; each component's layers
    /// are normalized so its leftmost layer sits at x = 0. Recomputed per frame
    /// so a grown component only ever pushes later components further down.
    ///
    /// # Arguments
    ///
    /// * `state` - The graph state the layout was built from
    pub fn positions(&self, state: &GraphState) -> HashMap<NodeId, (f64, f64)> {
        // group the placed nodes by component root
        let mut comps: HashMap<usize, Vec<(&NodeId, Placement)>> = HashMap::new();
        for (id, placement) in &self.pos {
            // skip nodes the state no longer holds (merged away mid-frame)
            if !state.nodes.contains_key(id) {
                continue;
            }
            comps
                .entry(self.find(placement.comp))
                .or_default()
                .push((id, *placement));
        }
        // order the components by the discovery index of their earliest node
        let mut ordered: Vec<_> = comps.into_values().collect();
        for comp in &mut ordered {
            // deterministic order within the component too
            comp.sort_by_key(|(id, _)| {
                state.nodes.get(id).map(|node| node.seq).unwrap_or(u32::MAX)
            });
        }
        ordered.sort_by_key(|comp| {
            comp.first()
                .and_then(|(id, _)| state.nodes.get(id).map(|node| node.seq))
                .unwrap_or(u32::MAX)
        });
        // stack the components vertically, normalizing each one's extents
        let mut positions = HashMap::new();
        let mut y_offset = 0.0;
        for comp in ordered {
            // find the component's leftmost layer and tallest slot stack
            let min_layer = comp.iter().map(|(_, p)| p.layer).min().unwrap_or(0);
            let max_slot = comp.iter().map(|(_, p)| p.slot).max().unwrap_or(0);
            // place each node relative to the component's own origin
            for (id, placement) in &comp {
                let x = f64::from(placement.layer - min_layer) * LAYER_SPACING;
                let y = y_offset + f64::from(placement.slot) * SLOT_SPACING;
                positions.insert((*id).clone(), (x, y));
            }
            // the next component starts below this one's tallest stack
            y_offset += f64::from(max_slot + 1) * SLOT_SPACING + COMPONENT_GAP;
        }
        positions
    }

    /// Places one node into a component at a layer, taking the next free slot
    ///
    /// # Arguments
    ///
    /// * `id` - The node to place
    /// * `root` - The component root to place it in
    /// * `layer` - The layer to place it at
    fn place(&mut self, id: NodeId, root: usize, layer: i32) {
        // hand out the layer's next free slot, append-only
        let slot = self.next_slot.entry((root, layer)).or_insert(0);
        let placement = Placement {
            comp: root,
            layer,
            slot: *slot,
        };
        *slot += 1;
        self.sizes[root] += 1;
        self.pos.insert(id, placement);
    }

    /// Allocates a brand new component and returns its root index
    fn new_component(&mut self) -> usize {
        // a new component is its own union-find parent
        let root = self.parents.len();
        self.parents.push(root);
        self.sizes.push(0);
        root
    }

    /// Whether a component index's parent chain passes through a given root
    ///
    /// # Arguments
    ///
    /// * `comp` - The component index whose chain to walk
    /// * `via` - The (possibly re-parented) root to look for on the chain
    fn resolves_through(&self, comp: usize, via: usize) -> bool {
        // walk the parent chain, succeeding as soon as the root is crossed
        let mut current = comp;
        loop {
            if current == via {
                return true;
            }
            if self.parents[current] == current {
                return false;
            }
            current = self.parents[current];
        }
    }

    /// Finds a component index's current root (without path compression)
    ///
    /// # Arguments
    ///
    /// * `comp` - The component index to resolve
    fn find(&self, comp: usize) -> usize {
        // walk the parent chain to the root; chains stay short since merges
        // always re-parent one root directly onto the other
        let mut current = comp;
        while self.parents[current] != current {
            current = self.parents[current];
        }
        current
    }

    /// Merges the smaller of two components into the larger along a new edge
    ///
    /// The smaller component's nodes are relocated once: their layers shift so
    /// the edge's endpoints end up on adjacent layers, and each node takes the
    /// next free slot at its new layer in the surviving component.
    ///
    /// # Arguments
    ///
    /// * `from_root` - The root of the edge source's component
    /// * `to_root` - The root of the edge destination's component
    /// * `from` - The edge source's placement before the merge
    /// * `to` - The edge destination's placement before the merge
    fn merge_components(
        &mut self,
        from_root: usize,
        to_root: usize,
        from: &Placement,
        to: &Placement,
    ) {
        // keep the larger component in place and move the smaller one
        let (keep, moved, delta) = if self.sizes[from_root] >= self.sizes[to_root] {
            // the destination component moves: its `to` node should land one
            // layer right of the kept `from` node
            (from_root, to_root, (from.layer + 1) - to.layer)
        } else {
            // the source component moves: its `from` node should land one
            // layer left of the kept `to` node
            (to_root, from_root, (to.layer - 1) - from.layer)
        };
        // re-parent the moved root so lookups resolve to the kept component
        self.parents[moved] = keep;
        self.sizes[keep] += self.sizes[moved];
        self.sizes[moved] = 0;
        // relocate exactly the nodes whose component resolves through the
        // moved root, in a deterministic order
        let mut moving: Vec<_> = self
            .pos
            .iter()
            .filter(|(_, placement)| self.resolves_through(placement.comp, moved))
            .map(|(id, placement)| (id.clone(), *placement))
            .collect();
        moving.sort_by_key(|(_, placement)| (placement.layer, placement.slot));
        for (id, old) in moving {
            // shift the node's layer and take a fresh slot in the kept component
            let layer = old.layer + delta;
            let slot = self.next_slot.entry((keep, layer)).or_insert(0);
            let placement = Placement {
                comp: keep,
                layer,
                slot: *slot,
            };
            *slot += 1;
            self.pos.insert(id, placement);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::TraversalEvent;
    use super::*;

    /// A function-scoped tool call event targeting a function
    fn call(call_id: &str, target: &str) -> TraversalEvent {
        TraversalEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: call_id.to_owned(),
            tool: "decomp_function".to_owned(),
            args: format!(r#"{{"target":"{target}"}}"#),
            ts: None,
        }
    }

    /// Placed nodes never move as new isolated nodes arrive
    #[test]
    fn placement_is_append_only() {
        let mut state = GraphState::default();
        let mut layout = Layout::default();
        // place the first node and remember where it landed
        state.apply(&call("c1", "alpha"));
        layout.update(&state, &BTreeSet::new());
        let before = layout.positions(&state);
        let alpha = NodeId::Name("alpha".to_owned());
        let first = before.get(&alpha).copied().expect("alpha placed");
        // add two more isolated nodes
        state.apply(&call("c2", "beta"));
        state.apply(&call("c3", "gamma"));
        layout.update(&state, &BTreeSet::new());
        // alpha kept its exact position; the others landed below it
        let after = layout.positions(&state);
        assert_eq!(after.get(&alpha).copied(), Some(first));
        assert_eq!(after.len(), 3);
        let beta = after
            .get(&NodeId::Name("beta".to_owned()))
            .expect("beta placed");
        assert!(beta.1 > first.1);
    }

    /// An edge joining two components relocates only the smaller one
    #[test]
    fn edge_unions_components() {
        let mut state = GraphState::default();
        let mut layout = Layout::default();
        // two isolated nodes become two components
        state.apply(&call("c1", "alpha"));
        state.apply(&call("c2", "beta"));
        layout.update(&state, &BTreeSet::new());
        // an enrichment edge alpha -> beta joins them
        let mut extra = BTreeSet::new();
        extra.insert((
            NodeId::Name("alpha".to_owned()),
            NodeId::Name("beta".to_owned()),
        ));
        layout.update(&state, &extra);
        // beta now sits one layer right of alpha
        let positions = layout.positions(&state);
        let alpha = positions
            .get(&NodeId::Name("alpha".to_owned()))
            .expect("alpha");
        let beta = positions
            .get(&NodeId::Name("beta".to_owned()))
            .expect("beta");
        assert!((beta.0 - alpha.0 - LAYER_SPACING).abs() < f64::EPSILON);
    }

    /// A node arriving with a known edge attaches next to its placed neighbor
    #[test]
    fn new_nodes_attach_to_placed_neighbors() {
        let mut state = GraphState::default();
        let mut layout = Layout::default();
        // visit a function, then learn its callee from a result
        state.apply(&call("c1", "alpha"));
        layout.update(&state, &BTreeSet::new());
        state.apply(&TraversalEvent::ToolCall {
            agent: "main".to_owned(),
            call_id: "c2".to_owned(),
            tool: "callees".to_owned(),
            args: r#"{"target":"alpha"}"#.to_owned(),
            ts: None,
        });
        state.apply(&TraversalEvent::ToolResult {
            agent: "main".to_owned(),
            call_id: "c2".to_owned(),
            tool: "callees".to_owned(),
            result: r#"{"function":"alpha","callees":[
                {"name":"helper","address":"00401200","call_site":"00401010","depth":0}
            ],"count":1}"#
                .to_owned(),
            success: true,
            ts: None,
        });
        layout.update(&state, &BTreeSet::new());
        // the callee landed one layer right of alpha, in the same component
        let positions = layout.positions(&state);
        let alpha = positions
            .get(&NodeId::Name("alpha".to_owned()))
            .expect("alpha");
        let helper = positions
            .get(&NodeId::Addr("401200".to_owned()))
            .expect("helper");
        assert!((helper.0 - alpha.0 - LAYER_SPACING).abs() < f64::EPSILON);
    }
}
