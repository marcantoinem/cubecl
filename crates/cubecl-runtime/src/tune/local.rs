use super::{AutotuneKey, AutotuneOutput, TunableSet, Tuner};
use crate::{
    channel::ComputeChannel, client::ComputeClient, server::ComputeServer, tune::TuneCacheResult,
};
use alloc::sync::Arc;
use core::{
    any::{Any, TypeId},
    fmt::Display,
    hash::Hash,
};
use hashbrown::HashMap;

#[cfg(not(feature = "std"))]
use alloc::string::ToString;

/// A local tuner allows to create a tuner for a specific key that can be different from the server
/// key.
pub struct LocalTuner<AK: AutotuneKey, ID> {
    state: spin::RwLock<Option<HashMap<ID, Tuner<AK>>>>,
    name: &'static str,
    sets: spin::RwLock<Option<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
}

unsafe impl<AK: AutotuneKey, ID> Sync for LocalTuner<AK, ID> {}

/// Create a local tuner with the provided name.
#[macro_export]
macro_rules! local_tuner {
    ($name:expr) => {
        LocalTuner::new(concat!(module_path!(), "-", $name));
    };
    () => {
        LocalTuner::new(module_path!());
    };
}

pub use local_tuner;

impl<AK, ID> LocalTuner<AK, ID>
where
    AK: AutotuneKey + 'static,
    ID: Hash + PartialEq + Eq + Clone + Display,
{
    /// Create a new local tuner.
    pub const fn new(name: &'static str) -> Self {
        Self {
            state: spin::RwLock::new(None),
            name,
            sets: spin::RwLock::new(None),
        }
    }

    /// Init the [tunable set](TunableSet)
    pub fn init<In, Out, F>(&self, init_set: F) -> Arc<TunableSet<AK, In, Out>>
    where
        F: Fn() -> TunableSet<AK, In, Out> + 'static + Send + Sync,
        In: Clone + Send + 'static,
        Out: AutotuneOutput,
    {
        let sets = self.sets.read();
        let type_id = TypeId::of::<F>();

        static DOWNCAST_ERROR: &str = "Local tuner only support one set of tunable that must work on the same input and output declared with the init function.";

        if let Some(sets) = sets.as_ref() {
            if let Some(set) = sets.get(&type_id) {
                return set.clone().downcast().expect(DOWNCAST_ERROR);
            }
        };
        core::mem::drop(sets);

        let mut sets = self.sets.write();
        let content = Arc::new(init_set());

        if let Some(sets) = sets.as_mut() {
            sets.insert(type_id, content.clone());
        } else {
            let mut map = HashMap::<TypeId, Arc<dyn Any + Send + Sync>>::new();
            map.insert(type_id, content.clone());
            *sets = Some(map);
        };

        content
    }

    /// Clear the autotune state.
    pub fn clear(&self) {
        let mut state = self.state.write();
        *state = None;
    }

    #[cfg(feature = "autotune-checks")]
    fn checks<In: Send + Clone + 'static, Out: AutotuneOutput>(
        &self,
        operations: &TunableSet<AK, In, Out>,
        inputs: &In,
    ) {
        let mut checks_outputs = Vec::new();
        for i in 0..operations.len() {
            let op = operations.fastest(i);
            let result = op.execute(inputs.clone());
            checks_outputs.push(result);
        }
        super::check_autotune_outputs(checks_outputs);
    }

    /// Execute the best operation in the provided [tunable set](TunableSet)
    pub fn execute<S, C, In, Out>(
        &self,
        id: &ID,
        client: &ComputeClient<S, C>,
        operations: Arc<TunableSet<AK, In, Out>>,
        inputs: In,
    ) -> Out
    where
        S: ComputeServer + 'static,
        C: ComputeChannel<S> + 'static,
        In: Clone + Send + 'static,
        Out: AutotuneOutput,
    {
        let key = operations.generate_key(&inputs);

        // If this is cached and ready, use the operation.
        if let Some(map) = self.state.read().as_ref() {
            if let Some(tuner) = map.get(id) {
                if let TuneCacheResult::Hit { fastest_index } = tuner.fastest(&key) {
                    #[cfg(feature = "autotune-checks")]
                    self.checks(&operations, &inputs);

                    let op = operations.fastest(fastest_index);
                    let result = op
                        .execute(inputs)
                        .expect("Should run when selected by autotune.");

                    return result;
                }
            }
        }

        // Create the tuner if needed, and update some state like
        // checksums if need be.
        let (fastest, run_autotune) = {
            let mut state = self.state.write();
            let map = state.get_or_insert_with(Default::default);
            let tuner = map.entry(id.clone()).or_insert_with(move || {
                let name = self.name.replace("::", "-");
                Tuner::new(&name, &id.to_string())
            });

            #[allow(unused_mut)]
            let mut fastest = tuner.fastest(&key);

            // If the cache checksum hasn't been checked, do so now, and retry.
            #[cfg(std_io)]
            if matches!(fastest, TuneCacheResult::Unchecked) {
                let checksum = operations.compute_checksum();
                tuner.validate_checksum(&key, &checksum);
                fastest = tuner.fastest(&key);
            }
            let mut run_autotune = false;

            if matches!(fastest, TuneCacheResult::Miss) && !tuner.autotuning.contains(&key) {
                tuner.autotuning.insert(key.clone());
                run_autotune = true;
            }
            (fastest, run_autotune)
        };

        match fastest {
            TuneCacheResult::Hit { fastest_index } => {
                #[cfg(feature = "autotune-checks")]
                self.checks(&operations, &inputs);

                return operations
                    .fastest(fastest_index)
                    .execute(inputs)
                    .expect("Should run when selected by autotune.");
            }
            TuneCacheResult::Miss => {
                if run_autotune {
                    // We don't know the results yet, start autotuning.
                    //
                    // Running benchmarks should't lock the tuner, since an autotune operation can recursively use the
                    // same tuner.
                    //
                    // # Example
                    //
                    // ```
                    // - tune_1 start
                    //   - tune_2 start
                    //   - tune_2 save
                    // - tune_1 save
                    // ```
                    let state = self.state.read();
                    let tuner = state
                        .as_ref()
                        .and_then(|s| s.get(id))
                        .expect("Should be initialized");
                    tuner.execute_autotune(key.clone(), &inputs, &operations, client);
                } else {
                    // We're waiting for results to come in.
                }
            }
            TuneCacheResult::Pending => {
                // We're waiting for results to come in.
            }
            TuneCacheResult::Unchecked => {
                panic!("Should have checked the cache already.")
            }
        };

        let fastest = {
            let mut state = self.state.write();
            let tuner = state
                .as_mut()
                .and_then(|s| s.get_mut(id))
                .expect("Should be initialized");

            // Read all results that have come in since.
            tuner.handle_results();

            // Check again what the fastest option is, new results might have come in.
            match tuner.fastest(&key) {
                TuneCacheResult::Hit { fastest_index } => {
                    // Theres a known good value - just run it.
                    fastest_index
                }
                TuneCacheResult::Pending => {
                    // If we still don't know, just execute a default index.
                    0
                }
                TuneCacheResult::Miss => {
                    if run_autotune {
                        panic!("Should have at least started autotuning");
                    } else {
                        // We're still waiting for the results of the autotune task.
                        // Let's execute the default index while we wait.
                        //
                        // This should only happen on wasm since we can't block waiting on the results there.
                        0
                    }
                }
                TuneCacheResult::Unchecked => {
                    panic!("Should have checked the cache.")
                }
            }
        };

        #[cfg(feature = "autotune-checks")]
        self.checks(&operations, &inputs);

        operations
            .fastest(fastest)
            .execute(inputs)
            .expect("Should run when selected by autotune.")
    }
}
