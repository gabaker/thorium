//! An in-memory graph the workflow resolver can read, for tests
//!
//! Every edge is stored as two rows, one on each end, the way Thorium stores associations, and
//! every read is recorded so tests can check what the resolver asked for.

use std::collections::HashMap;
use std::sync::Mutex;
use uuid::Uuid;

use super::{GraphEdge, GraphNode, GraphReader};
use crate::Error;
use crate::models::{AssociationKind, Directionality, Entity, EntityKinds};

/// One read a graph reader was asked for
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphRead {
    /// An entity read by id with its edges of some kinds in one direction
    Node {
        /// The entity that was read
        id: Uuid,
        /// The edge kinds that were asked for
        kinds: Vec<AssociationKind>,
        /// The direction of the edges that were asked for
        direction: Directionality,
    },
    /// A tag lookup
    Tagged {
        /// The entity kind that was looked up
        kind: EntityKinds,
        /// The tag key
        key: String,
        /// The tag value
        value: String,
        /// The groups that were searched
        groups: Vec<String>,
    },
}

/// An in-memory graph of entities and their edges that records every read
#[derive(Debug, Default)]
pub struct MemoryGraph {
    /// Every entity by id
    entities: HashMap<Uuid, Entity>,
    /// Every edge row stored on each entity, in the order they were added
    rows: HashMap<Uuid, Vec<GraphEdge>>,
    /// Whether node reads return rows in every direction instead of only the one asked for
    every_direction: bool,
    /// Every read made so far
    reads: Mutex<Vec<GraphRead>>,
    /// The entities whose reads fail, with how each fails
    failing: HashMap<Uuid, ReadFailure>,
    /// How every tag lookup fails, if tag lookups fail
    failing_tags: Option<ReadFailure>,
}

/// How an injected read failure fails
#[derive(Debug, Clone, Copy)]
enum ReadFailure {
    /// The API answers with an HTTP status
    Status(u16),
    /// The API can't be reached
    Connection,
}

impl From<Option<u16>> for ReadFailure {
    /// Get the failure a status, or no status, describes
    ///
    /// # Arguments
    ///
    /// * `status` - The HTTP status the failure carries, or none for a connection error
    fn from(status: Option<u16>) -> Self {
        // a status is an API error and no status is a connection error
        status.map_or(ReadFailure::Connection, ReadFailure::Status)
    }
}

/// Build the error a failing read returns
///
/// # Arguments
///
/// * `failure` - How the read fails
fn read_failure(failure: ReadFailure) -> Error {
    // a status is an API error and anything else is a connection error
    let status = match failure {
        ReadFailure::Status(code) => reqwest::StatusCode::from_u16(code).ok(),
        ReadFailure::Connection => None,
    };
    match status {
        Some(code) => Error::Thorium {
            code,
            msg: Some("injected read failure".to_owned()),
        },
        None => Error::new("injected connection error"),
    }
}

impl MemoryGraph {
    /// Make every read of an entity fail
    ///
    /// # Arguments
    ///
    /// * `id` - The entity whose reads fail
    /// * `status` - The HTTP status each failure carries, or none for a connection error
    pub fn fail_reads(&mut self, id: Uuid, status: Option<u16>) {
        // remember the failure for this entity
        self.failing.insert(id, ReadFailure::from(status));
    }

    /// Make an entity's reads succeed again after they were made to fail
    ///
    /// # Arguments
    ///
    /// * `id` - The entity whose reads succeed again
    pub fn recover_reads(&mut self, id: Uuid) {
        // forget the failure for this entity
        self.failing.remove(&id);
    }

    /// Make every tag lookup fail
    ///
    /// # Arguments
    ///
    /// * `status` - The HTTP status each failure carries, or none for a connection error
    pub fn fail_tag_lookups(&mut self, status: Option<u16>) {
        // remember the failure for every lookup
        self.failing_tags = Some(ReadFailure::from(status));
    }

    /// Add or replace an entity
    ///
    /// # Arguments
    ///
    /// * `entity` - The entity to add
    pub fn insert(&mut self, entity: Entity) {
        // store the entity by its id
        self.entities.insert(entity.id, entity);
    }

    /// Get a stored entity
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity
    #[must_use]
    pub fn entity(&self, id: Uuid) -> Option<&Entity> {
        // look the entity up by id
        self.entities.get(&id)
    }

    /// Get a stored entity to change it
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity
    pub fn entity_mut(&mut self, id: Uuid) -> Option<&mut Entity> {
        // look the entity up by id
        self.entities.get_mut(&id)
    }

    /// Get the kind of a stored entity, or `Other` for an entity that isn't stored
    ///
    /// # Arguments
    ///
    /// * `id` - The id of the entity
    fn kind_of(&self, id: Uuid) -> EntityKinds {
        // an entity that isn't stored has no kind of its own
        self.entities
            .get(&id)
            .map_or(EntityKinds::Other, |entity| entity.kind)
    }

    /// Add an edge as its two rows: one leaving the source and one arriving at the target
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of edge
    /// * `source` - The entity the edge starts at
    /// * `target` - The entity the edge points to
    pub fn edge(&mut self, kind: AssociationKind, source: Uuid, target: Uuid) {
        // the source's row points forward to the target
        let forward = GraphEdge {
            kind,
            direction: Directionality::To,
            other: target,
            other_kind: self.kind_of(target),
        };
        // the target's row points back at the source
        let reverse = GraphEdge {
            kind,
            direction: Directionality::From,
            other: source,
            other_kind: self.kind_of(source),
        };
        self.rows.entry(source).or_default().push(forward);
        self.rows.entry(target).or_default().push(reverse);
    }

    /// Add a single row to an entity without its partner row
    ///
    /// # Arguments
    ///
    /// * `at` - The entity the row is stored on
    /// * `row` - The row to store
    pub fn row(&mut self, at: Uuid, row: GraphEdge) {
        // store just this row
        self.rows.entry(at).or_default().push(row);
    }

    /// Remove every edge of a kind between two entities, both rows
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of edge
    /// * `source` - The entity the edge starts at
    /// * `target` - The entity the edge points to
    pub fn remove_edge(&mut self, kind: AssociationKind, source: Uuid, target: Uuid) {
        // drop the source's forward rows and the target's reverse rows
        let matches = |row: &GraphEdge, direction: Directionality, other: Uuid| {
            row.kind == kind && row.direction == direction && row.other == other
        };
        if let Some(rows) = self.rows.get_mut(&source) {
            rows.retain(|row| !matches(row, Directionality::To, target));
        }
        if let Some(rows) = self.rows.get_mut(&target) {
            rows.retain(|row| !matches(row, Directionality::From, source));
        }
    }

    /// Reverse the order every entity's rows are returned in
    pub fn reverse_row_order(&mut self) {
        // flip each entity's rows
        for rows in self.rows.values_mut() {
            rows.reverse();
        }
    }

    /// Return rows in every direction from node reads, as a careless reader would
    ///
    /// # Arguments
    ///
    /// * `every_direction` - Whether to ignore the direction a read asks for
    pub fn return_every_direction(&mut self, every_direction: bool) {
        // node reads check this before filtering by direction
        self.every_direction = every_direction;
    }

    /// Tag an entity in its groups
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to tag
    /// * `key` - The tag key
    /// * `value` - The tag value
    pub fn tag(&mut self, id: Uuid, key: &str, value: &str) {
        // tag the entity in each of its groups
        if let Some(entity) = self.entities.get_mut(&id) {
            let groups = entity.groups.iter().cloned().collect();
            entity
                .tags
                .entry(key.to_owned())
                .or_default()
                .insert(value.to_owned(), groups);
        }
    }

    /// Tag an entity in some groups only
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to tag
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to tag it in
    pub fn tag_in(&mut self, id: Uuid, key: &str, value: &str, groups: &[&str]) {
        // tag the entity in just these groups
        if let Some(entity) = self.entities.get_mut(&id) {
            let groups = groups.iter().map(|group| (*group).to_owned()).collect();
            entity
                .tags
                .entry(key.to_owned())
                .or_default()
                .insert(value.to_owned(), groups);
        }
    }

    /// Get every read made so far
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while recording a read.
    #[must_use]
    pub fn reads(&self) -> Vec<GraphRead> {
        // copy the log
        self.reads.lock().expect("graph read log poisoned").clone()
    }

    /// Forget every read made so far
    ///
    /// # Panics
    ///
    /// Panics if another thread panicked while recording a read.
    pub fn clear_reads(&self) {
        // empty the log
        self.reads.lock().expect("graph read log poisoned").clear();
    }

    /// Count the node reads of one entity
    ///
    /// # Arguments
    ///
    /// * `id` - The entity whose reads to count
    #[must_use]
    pub fn node_reads(&self, id: Uuid) -> usize {
        // count the node reads naming this entity
        self.reads()
            .iter()
            .filter(|read| matches!(read, GraphRead::Node { id: read_id, .. } if *read_id == id))
            .count()
    }

    /// Record a read
    ///
    /// # Arguments
    ///
    /// * `read` - The read to record
    fn record(&self, read: GraphRead) {
        // a poisoned log only means another test thread panicked, so keep recording
        let mut reads = match self.reads.lock() {
            Ok(reads) => reads,
            Err(poisoned) => poisoned.into_inner(),
        };
        reads.push(read);
    }
}

#[async_trait::async_trait]
impl GraphReader for MemoryGraph {
    /// Read an entity by id with its stored rows of some kinds in one direction
    ///
    /// # Arguments
    ///
    /// * `id` - The entity to read
    /// * `kinds` - The edge kinds to return
    /// * `direction` - The direction of the edges to return
    async fn node(
        &self,
        id: Uuid,
        kinds: &[AssociationKind],
        direction: Directionality,
    ) -> Result<Option<GraphNode>, Error> {
        // record what was asked for
        self.record(GraphRead::Node {
            id,
            kinds: kinds.to_vec(),
            direction,
        });
        // a failing entity's read fails
        if let Some(status) = self.failing.get(&id) {
            return Err(read_failure(*status));
        }
        // an entity that isn't stored doesn't exist
        let Some(entity) = self.entities.get(&id) else {
            return Ok(None);
        };
        // return the rows of the asked kinds, in the asked direction unless told otherwise
        let edges = match self.rows.get(&id) {
            Some(rows) => rows
                .iter()
                .filter(|row| kinds.contains(&row.kind))
                .filter(|row| self.every_direction || row.direction == direction)
                .cloned()
                .collect(),
            None => Vec::new(),
        };
        Ok(Some(GraphNode {
            entity: entity.clone(),
            edges,
        }))
    }

    /// Find the entities of a kind tagged with a value in any of some groups
    ///
    /// # Arguments
    ///
    /// * `kind` - The kind of entity to find
    /// * `key` - The tag key
    /// * `value` - The tag value
    /// * `groups` - The groups to search
    async fn tagged(
        &self,
        kind: EntityKinds,
        key: &str,
        value: &str,
        groups: &[String],
    ) -> Result<Vec<Entity>, Error> {
        // record what was asked for
        self.record(GraphRead::Tagged {
            kind,
            key: key.to_owned(),
            value: value.to_owned(),
            groups: groups.to_vec(),
        });
        // a failing lookup fails
        if let Some(status) = self.failing_tags {
            return Err(read_failure(status));
        }
        // keep the entities of this kind tagged in one of the groups
        let mut found = self
            .entities
            .values()
            .filter(|entity| entity.kind == kind)
            .filter(|entity| {
                entity
                    .tags
                    .get(key)
                    .and_then(|values| values.get(value))
                    .is_some_and(|tagged| groups.iter().any(|group| tagged.contains(group)))
            })
            .cloned()
            .collect::<Vec<Entity>>();
        // list the newest first, as the API does, and entities created together by id
        found.sort_by(|left, right| {
            right
                .created
                .cmp(&left.created)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(found)
    }
}
