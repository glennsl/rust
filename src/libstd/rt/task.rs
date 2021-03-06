// Copyright 2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Language-level runtime services that should reasonably expected
//! to be available 'everywhere'. Local heaps, GC, unwinding,
//! local storage, and logging. Even a 'freestanding' Rust would likely want
//! to implement this.

use borrow;
use cast::transmute;
use cleanup;
use local_data;
use libc::{c_void, uintptr_t, c_char, size_t};
use prelude::*;
use option::{Option, Some, None};
use rt::borrowck;
use rt::borrowck::BorrowRecord;
use rt::env;
use rt::kill::Death;
use rt::local::Local;
use rt::logging::StdErrLogger;
use super::local_heap::LocalHeap;
use rt::sched::{Scheduler, SchedHandle};
use rt::stack::{StackSegment, StackPool};
use rt::context::Context;
use unstable::finally::Finally;
use task::spawn::Taskgroup;
use cell::Cell;
use send_str::SendStr;

// The Task struct represents all state associated with a rust
// task. There are at this point two primary "subtypes" of task,
// however instead of using a subtype we just have a "task_type" field
// in the struct. This contains a pointer to another struct that holds
// the type-specific state.

pub struct Task {
    heap: LocalHeap,
    gc: GarbageCollector,
    storage: LocalStorage,
    logger: StdErrLogger,
    unwinder: Unwinder,
    taskgroup: Option<Taskgroup>,
    death: Death,
    destroyed: bool,
    name: Option<SendStr>,
    coroutine: Option<Coroutine>,
    sched: Option<~Scheduler>,
    task_type: TaskType,
    // Dynamic borrowck debugging info
    borrow_list: Option<~[BorrowRecord]>
}

pub enum TaskType {
    GreenTask(Option<SchedHome>),
    SchedTask
}

/// A coroutine is nothing more than a (register context, stack) pair.
pub struct Coroutine {
    /// The segment of stack on which the task is currently running or
    /// if the task is blocked, on which the task will resume
    /// execution.
    current_stack_segment: StackSegment,
    /// Always valid if the task is alive and not running.
    saved_context: Context
}

/// Some tasks have a dedicated home scheduler that they must run on.
pub enum SchedHome {
    AnySched,
    Sched(SchedHandle)
}

pub struct GarbageCollector;
pub struct LocalStorage(Option<local_data::Map>);

pub struct Unwinder {
    unwinding: bool,
}

impl Task {

    // A helper to build a new task using the dynamically found
    // scheduler and task. Only works in GreenTask context.
    pub fn build_homed_child(stack_size: Option<uint>, f: ~fn(), home: SchedHome) -> ~Task {
        let f = Cell::new(f);
        let home = Cell::new(home);
        do Local::borrow |running_task: &mut Task| {
            let mut sched = running_task.sched.take_unwrap();
            let new_task = ~running_task.new_child_homed(&mut sched.stack_pool,
                                                         stack_size,
                                                         home.take(),
                                                         f.take());
            running_task.sched = Some(sched);
            new_task
        }
    }

    pub fn build_child(stack_size: Option<uint>, f: ~fn()) -> ~Task {
        Task::build_homed_child(stack_size, f, AnySched)
    }

    pub fn build_homed_root(stack_size: Option<uint>, f: ~fn(), home: SchedHome) -> ~Task {
        let f = Cell::new(f);
        let home = Cell::new(home);
        do Local::borrow |running_task: &mut Task| {
            let mut sched = running_task.sched.take_unwrap();
            let new_task = ~Task::new_root_homed(&mut sched.stack_pool,
                                                 stack_size,
                                                 home.take(),
                                                 f.take());
            running_task.sched = Some(sched);
            new_task
        }
    }

    pub fn build_root(stack_size: Option<uint>, f: ~fn()) -> ~Task {
        Task::build_homed_root(stack_size, f, AnySched)
    }

    pub fn new_sched_task() -> Task {
        Task {
            heap: LocalHeap::new(),
            gc: GarbageCollector,
            storage: LocalStorage(None),
            logger: StdErrLogger,
            unwinder: Unwinder { unwinding: false },
            taskgroup: None,
            death: Death::new(),
            destroyed: false,
            coroutine: Some(Coroutine::empty()),
            name: None,
            sched: None,
            task_type: SchedTask,
            borrow_list: None
        }
    }

    pub fn new_root(stack_pool: &mut StackPool,
                    stack_size: Option<uint>,
                    start: ~fn()) -> Task {
        Task::new_root_homed(stack_pool, stack_size, AnySched, start)
    }

    pub fn new_child(&mut self,
                     stack_pool: &mut StackPool,
                     stack_size: Option<uint>,
                     start: ~fn()) -> Task {
        self.new_child_homed(stack_pool, stack_size, AnySched, start)
    }

    pub fn new_root_homed(stack_pool: &mut StackPool,
                          stack_size: Option<uint>,
                          home: SchedHome,
                          start: ~fn()) -> Task {
        Task {
            heap: LocalHeap::new(),
            gc: GarbageCollector,
            storage: LocalStorage(None),
            logger: StdErrLogger,
            unwinder: Unwinder { unwinding: false },
            taskgroup: None,
            death: Death::new(),
            destroyed: false,
            name: None,
            coroutine: Some(Coroutine::new(stack_pool, stack_size, start)),
            sched: None,
            task_type: GreenTask(Some(home)),
            borrow_list: None
        }
    }

    pub fn new_child_homed(&mut self,
                           stack_pool: &mut StackPool,
                           stack_size: Option<uint>,
                           home: SchedHome,
                           start: ~fn()) -> Task {
        Task {
            heap: LocalHeap::new(),
            gc: GarbageCollector,
            storage: LocalStorage(None),
            logger: StdErrLogger,
            unwinder: Unwinder { unwinding: false },
            taskgroup: None,
            // FIXME(#7544) make watching optional
            death: self.death.new_child(),
            destroyed: false,
            name: None,
            coroutine: Some(Coroutine::new(stack_pool, stack_size, start)),
            sched: None,
            task_type: GreenTask(Some(home)),
            borrow_list: None
        }
    }

    pub fn give_home(&mut self, new_home: SchedHome) {
        match self.task_type {
            GreenTask(ref mut home) => {
                *home = Some(new_home);
            }
            SchedTask => {
                rtabort!("type error: used SchedTask as GreenTask");
            }
        }
    }

    pub fn take_unwrap_home(&mut self) -> SchedHome {
        match self.task_type {
            GreenTask(ref mut home) => {
                let out = home.take_unwrap();
                return out;
            }
            SchedTask => {
                rtabort!("type error: used SchedTask as GreenTask");
            }
        }
    }

    pub fn run(&mut self, f: &fn()) {
        rtdebug!("run called on task: {}", borrow::to_uint(self));

        // The only try/catch block in the world. Attempt to run the task's
        // client-specified code and catch any failures.
        do self.unwinder.try {

            // Run the task main function, then do some cleanup.
            do f.finally {
                // First, destroy task-local storage. This may run user dtors.
                //
                // FIXME #8302: Dear diary. I'm so tired and confused.
                // There's some interaction in rustc between the box
                // annihilator and the TLS dtor by which TLS is
                // accessed from annihilated box dtors *after* TLS is
                // destroyed. Somehow setting TLS back to null, as the
                // old runtime did, makes this work, but I don't currently
                // understand how. I would expect that, if the annihilator
                // reinvokes TLS while TLS is uninitialized, that
                // TLS would be reinitialized but never destroyed,
                // but somehow this works. I have no idea what's going
                // on but this seems to make things magically work. FML.
                //
                // (added after initial comment) A possible interaction here is
                // that the destructors for the objects in TLS themselves invoke
                // TLS, or possibly some destructors for those objects being
                // annihilated invoke TLS. Sadly these two operations seemed to
                // be intertwined, and miraculously work for now...
                self.storage.take();

                // Destroy remaining boxes. Also may run user dtors.
                unsafe { cleanup::annihilate(); }
            }
        }

        // Cleanup the dynamic borrowck debugging info
        borrowck::clear_task_borrow_list();

        // NB. We pass the taskgroup into death so that it can be dropped while
        // the unkillable counter is set. This is necessary for when the
        // taskgroup destruction code drops references on KillHandles, which
        // might require using unkillable (to synchronize with an unwrapper).
        self.death.collect_failure(!self.unwinder.unwinding, self.taskgroup.take());
        self.destroyed = true;
    }

    // New utility functions for homes.

    pub fn is_home_no_tls(&self, sched: &~Scheduler) -> bool {
        match self.task_type {
            GreenTask(Some(AnySched)) => { false }
            GreenTask(Some(Sched(SchedHandle { sched_id: ref id, _}))) => {
                *id == sched.sched_id()
            }
            GreenTask(None) => {
                rtabort!("task without home");
            }
            SchedTask => {
                // Awe yea
                rtabort!("type error: expected: GreenTask, found: SchedTask");
            }
        }
    }

    pub fn homed(&self) -> bool {
        match self.task_type {
            GreenTask(Some(AnySched)) => { false }
            GreenTask(Some(Sched(SchedHandle { _ }))) => { true }
            GreenTask(None) => {
                rtabort!("task without home");
            }
            SchedTask => {
                rtabort!("type error: expected: GreenTask, found: SchedTask");
            }
        }
    }

    // Grab both the scheduler and the task from TLS and check if the
    // task is executing on an appropriate scheduler.
    pub fn on_appropriate_sched() -> bool {
        do Local::borrow |task: &mut Task| {
            let sched_id = task.sched.get_ref().sched_id();
            let sched_run_anything = task.sched.get_ref().run_anything;
            match task.task_type {
                GreenTask(Some(AnySched)) => {
                    rtdebug!("anysched task in sched check ****");
                    sched_run_anything
                }
                GreenTask(Some(Sched(SchedHandle { sched_id: ref id, _ }))) => {
                    rtdebug!("homed task in sched check ****");
                    *id == sched_id
                }
                GreenTask(None) => {
                    rtabort!("task without home");
                }
                SchedTask => {
                    rtabort!("type error: expected: GreenTask, found: SchedTask");
                }
            }
        }
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        rtdebug!("called drop for a task: {}", borrow::to_uint(self));
        rtassert!(self.destroyed)
    }
}

// Coroutines represent nothing more than a context and a stack
// segment.

impl Coroutine {

    pub fn new(stack_pool: &mut StackPool, stack_size: Option<uint>, start: ~fn()) -> Coroutine {
        let stack_size = match stack_size {
            Some(size) => size,
            None => env::min_stack()
        };
        let start = Coroutine::build_start_wrapper(start);
        let mut stack = stack_pool.take_segment(stack_size);
        let initial_context = Context::new(start, &mut stack);
        Coroutine {
            current_stack_segment: stack,
            saved_context: initial_context
        }
    }

    pub fn empty() -> Coroutine {
        Coroutine {
            current_stack_segment: StackSegment::new(0),
            saved_context: Context::empty()
        }
    }

    fn build_start_wrapper(start: ~fn()) -> ~fn() {
        let start_cell = Cell::new(start);
        let wrapper: ~fn() = || {
            // First code after swap to this new context. Run our
            // cleanup job.
            unsafe {

                // Again - might work while safe, or it might not.
                do Local::borrow |sched: &mut Scheduler| {
                    sched.run_cleanup_job();
                }

                // To call the run method on a task we need a direct
                // reference to it. The task is in TLS, so we can
                // simply unsafe_borrow it to get this reference. We
                // need to still have the task in TLS though, so we
                // need to unsafe_borrow.
                let task: *mut Task = Local::unsafe_borrow();

                do (*task).run {
                    // N.B. Removing `start` from the start wrapper
                    // closure by emptying a cell is critical for
                    // correctness. The ~Task pointer, and in turn the
                    // closure used to initialize the first call
                    // frame, is destroyed in the scheduler context,
                    // not task context. So any captured closures must
                    // not contain user-definable dtors that expect to
                    // be in task context. By moving `start` out of
                    // the closure, all the user code goes our of
                    // scope while the task is still running.
                    let start = start_cell.take();
                    start();
                };
            }

            // We remove the sched from the Task in TLS right now.
            let sched: ~Scheduler = Local::take();
            // ... allowing us to give it away when performing a
            // scheduling operation.
            sched.terminate_current_task()
        };
        return wrapper;
    }

    /// Destroy coroutine and try to reuse stack segment.
    pub fn recycle(self, stack_pool: &mut StackPool) {
        match self {
            Coroutine { current_stack_segment, _ } => {
                stack_pool.give_segment(current_stack_segment);
            }
        }
    }

}


// Just a sanity check to make sure we are catching a Rust-thrown exception
static UNWIND_TOKEN: uintptr_t = 839147;

impl Unwinder {
    pub fn try(&mut self, f: &fn()) {
        use unstable::raw::Closure;

        unsafe {
            let closure: Closure = transmute(f);
            let code = transmute(closure.code);
            let env = transmute(closure.env);

            let token = rust_try(try_fn, code, env);
            assert!(token == 0 || token == UNWIND_TOKEN);
        }

        extern fn try_fn(code: *c_void, env: *c_void) {
            unsafe {
                let closure: Closure = Closure {
                    code: transmute(code),
                    env: transmute(env),
                };
                let closure: &fn() = transmute(closure);
                closure();
            }
        }

        extern {
            #[rust_stack]
            fn rust_try(f: extern "C" fn(*c_void, *c_void),
                        code: *c_void,
                        data: *c_void) -> uintptr_t;
        }
    }

    pub fn begin_unwind(&mut self) -> ! {
        #[fixed_stack_segment]; #[inline(never)];

        self.unwinding = true;
        unsafe {
            rust_begin_unwind(UNWIND_TOKEN);
            return transmute(());
        }
        extern {
            fn rust_begin_unwind(token: uintptr_t);
        }
    }
}

/// This is the entry point of unwinding for things like lang items and such.
/// The arguments are normally generated by the compiler.
pub fn begin_unwind(msg: *c_char, file: *c_char, line: size_t) -> ! {
    use rt::in_green_task_context;
    use rt::task::Task;
    use rt::local::Local;
    use rt::logging::Logger;
    use str::Str;
    use c_str::CString;

    unsafe {
        let msg = CString::new(msg, false);
        let file = CString::new(file, false);
        let msg = match msg.as_str() {
            Some(s) => s, None => rtabort!("message wasn't utf8?")
        };
        let file = match file.as_str() {
            Some(s) => s, None => rtabort!("message wasn't utf8?")
        };

        if in_green_task_context() {
            // Be careful not to allocate in this block, if we're failing we may
            // have been failing due to a lack of memory in the first place...
            do Local::borrow |task: &mut Task| {
                let n = task.name.as_ref().map(|n| n.as_slice()).unwrap_or("<unnamed>");
                format_args!(|args| { task.logger.log(args) },
                             "task '{}' failed at '{}', {}:{}",
                             n, msg, file, line);
            }
        } else {
            rterrln!("failed in non-task context at '{}', {}:{}",
                     msg, file, line as int);
        }

        let task: *mut Task = Local::unsafe_borrow();
        if (*task).unwinder.unwinding {
            rtabort!("unwinding again");
        }
        (*task).unwinder.begin_unwind();
    }
}

#[cfg(test)]
mod test {
    use rt::test::*;

    #[test]
    fn local_heap() {
        do run_in_newsched_task() {
            let a = @5;
            let b = a;
            assert!(*a == 5);
            assert!(*b == 5);
        }
    }

    #[test]
    fn tls() {
        use local_data;
        do run_in_newsched_task() {
            local_data_key!(key: @~str)
            local_data::set(key, @~"data");
            assert!(*local_data::get(key, |k| k.map(|k| *k)).unwrap() == ~"data");
            local_data_key!(key2: @~str)
            local_data::set(key2, @~"data");
            assert!(*local_data::get(key2, |k| k.map(|k| *k)).unwrap() == ~"data");
        }
    }

    #[test]
    fn unwind() {
        do run_in_newsched_task() {
            let result = spawntask_try(||());
            rtdebug!("trying first assert");
            assert!(result.is_ok());
            let result = spawntask_try(|| fail2!());
            rtdebug!("trying second assert");
            assert!(result.is_err());
        }
    }

    #[test]
    fn rng() {
        do run_in_newsched_task() {
            use rand::{rng, Rng};
            let mut r = rng();
            let _ = r.next_u32();
        }
    }

    #[test]
    fn logging() {
        do run_in_newsched_task() {
            info2!("here i am. logging in a newsched task");
        }
    }

    #[test]
    fn comm_oneshot() {
        use comm::*;

        do run_in_newsched_task {
            let (port, chan) = oneshot();
            chan.send(10);
            assert!(port.recv() == 10);
        }
    }

    #[test]
    fn comm_stream() {
        use comm::*;

        do run_in_newsched_task() {
            let (port, chan) = stream();
            chan.send(10);
            assert!(port.recv() == 10);
        }
    }

    #[test]
    fn comm_shared_chan() {
        use comm::*;

        do run_in_newsched_task() {
            let (port, chan) = stream();
            let chan = SharedChan::new(chan);
            chan.send(10);
            assert!(port.recv() == 10);
        }
    }

    #[test]
    fn linked_failure() {
        do run_in_newsched_task() {
            let res = do spawntask_try {
                spawntask_random(|| fail2!());
            };
            assert!(res.is_err());
        }
    }

    #[test]
    fn heap_cycles() {
        use option::{Option, Some, None};

        do run_in_newsched_task {
            struct List {
                next: Option<@mut List>,
            }

            let a = @mut List { next: None };
            let b = @mut List { next: Some(a) };

            a.next = Some(b);
        }
    }

    // XXX: This is a copy of test_future_result in std::task.
    // It can be removed once the scheduler is turned on by default.
    #[test]
    fn future_result() {
        do run_in_newsched_task {
            use option::{Some, None};
            use task::*;

            let mut result = None;
            let mut builder = task();
            builder.future_result(|r| result = Some(r));
            do builder.spawn {}
            assert_eq!(result.unwrap().recv(), Success);

            result = None;
            let mut builder = task();
            builder.future_result(|r| result = Some(r));
            builder.unlinked();
            do builder.spawn {
                fail2!();
            }
            assert_eq!(result.unwrap().recv(), Failure);
        }
    }
}
