# Evidence for `xtask/labels/kernel.json`

Corpus: Linux at `1590cf032971` (7.2-rc4), graded against the checkout as-is. Paths are
relative to the tree; `file:line` is the definition, the quote is verbatim from the
kerneldoc / source comment / `Documentation/` entry that proves the grade-3 symbol is what
the query means. Grading is from the source only — never from what search returns.
Paraphrase queries share no token with the identifier (or its file name); descriptive
queries share at most one. `path` is recorded wherever a name is defined in more than one
file: a plain suffix matches with `ends_with`; a suffix starting with `/` is ANCHORED at
the tree root (tree-relative equality) — needed because `tools/` mirrors `lib/rbtree.c`,
`include/linux/slab.h` and friends, so no plain suffix can single them out.

## original set (graded 2026-08-30)

- **short-keyword** "socket buffer alloc" → `alloc_skb` (grade 3): include/linux/skbuff.h:1381 — kerneldoc :1375 "alloc_skb - allocate a network buffer". [grade 2 `__alloc_skb` (net/core/skbuff.c) — the worker; `sock_alloc_send_skb` — the socket-side allocator; grade 1 `socket_alloc`]
- **short-keyword** "page fault handler" → `handle_mm_fault` (grade 3): mm/memory.c:6651 — comment :6637 "By the time we get here, we already hold either the VMA lock or the mmap_lock (see FAULT_FLAG_VMA_LOCK)." — the arch-independent fault entry. [grade 2 `__handle_mm_fault`, `do_user_addr_fault` (x86 entry)]
- **short-keyword** "tcp congestion window" → `tcp_cong_avoid_ai` (grade 3): net/ipv4/tcp_cong.c:470 — comment :467 "In theory this is tp->snd_cwnd += 1 / tp->snd_cwnd (or alternative w), for every packet that was ACKed." [grade 2 `tcp_slow_start`, `tcp_cwnd_reduction`; grade 1 `tcp_cwnd_restart`]
- **short-keyword** "mutex lock acquire" → `mutex_lock` (grade 3): kernel/locking/mutex.c:314 — kerneldoc :294 "mutex_lock - acquire the mutex". [grade 2 `__mutex_lock`; grade 1 `mutex_trylock`, `mutex_lock_interruptible`]
- **short-keyword** "inode lookup path" → `path_lookupat` (grade 3): fs/namei.c:2801 — comment :2800 "Returns 0 and nd will be valid on success; Returns error, otherwise." — the pathname-walk driver. [grade 2 `lookup_fast`, `lookup_slow`]
- **short-keyword** "interrupt request register" → `request_threaded_irq` (grade 3): kernel/irq/manage.c:2123 — kerneldoc :2084 "request_threaded_irq - allocate an interrupt line". [`request_irq` grade 3 — the inline wrapper; grade 2 `__setup_irq`]
- **short-keyword** "dma coherent alloc" → `dma_alloc_coherent` (grade 3): include/linux/dma-mapping.h:614 — Documentation/core-api/dma-api.rst:33 "dma_alloc_coherent(struct device *dev, size_t size, ...)" (the consistent-memory allocator). [grade 2 `dma_alloc_attrs`, `dma_direct_alloc`]
- **descriptive** "scheduler pick next task" → `pick_next_task` (grade 3): kernel/sched/core.c:6216 (and the !SCHED_CORE variant :6671) — "static struct task_struct * pick_next_task(struct rq *rq, struct rq_flags *rf)". [grade 2 `__pick_next_task` :6124; grade 1 `pick_next_entity`]

## note on dropped candidates (2026-09-03)

Defined in source but ABSENT from the index at this commit (the existence gate would
fail the run), so not labelled: `hrtimer_interrupt` (kernel/time/hrtimer.c:2185),
`hrtimer_start_range_ns` (kernel/time/hrtimer.c:1493), `netif_receive_skb` and
`__netif_receive_skb` (net/core/dev.c:6464 / :6305), `kzalloc_noprof`
(include/linux/slab.h:1292).


## exact (added 2026-09-03)

- **exact** "bio_add_page" → `bio_add_page` (grade 3): block/bio.c:1047 (kerneldoc at 1038) — "bio_add_page	-	attempt to add page(s) to bio".
- **exact** "d_splice_alias" → `d_splice_alias` (grade 3): fs/dcache.c:3313 (kerneldoc at 3291) — "d_splice_alias - splice a disconnected dentry into the tree if one exists".
- **exact** "update_load_avg" → `update_load_avg` (grade 3): kernel/sched/fair.c:5642 (comment at 5641, static inline inside the giant fair.c) — "/* Update task and its cfs_rq load average */".
- **exact** "device_link_add" → `device_link_add` (grade 3): drivers/base/core.c:800 (kerneldoc at 742) — "device_link_add - Create a link between two devices."
- **exact** "tcp_ack_update_rtt" → `tcp_ack_update_rtt` (grade 3): net/ipv4/tcp_input.c:3459 (static, inside the giant tcp_input.c) — "static bool tcp_ack_update_rtt(struct sock *sk, const int flag,".
- **exact** "wq_worker_sleeping" → `wq_worker_sleeping` (grade 3): kernel/workqueue.c:1453 (kerneldoc at 1447) — "wq_worker_sleeping - a worker is going to sleep".
- **exact** "amdgpu_device_init" → `amdgpu_device_init` (grade 3): drivers/gpu/drm/amd/amdgpu/amdgpu_device.c:3709 (kerneldoc at 3700) — "amdgpu_device_init - initialize the driver".
- **exact** "xas_nomem" → `xas_nomem` (grade 3): lib/xarray.c:301 (kerneldoc at 284) — "xas_nomem() - Allocate memory if needed."

## short-keyword (added 2026-09-03)

- **short-keyword** "delayed work queue" → `queue_delayed_work` (grade 3): include/linux/workqueue.h:710 (kerneldoc at 703) — "queue_delayed_work - queue work on a workqueue after delay". Grade 2 `queue_delayed_work_on` (kernel/workqueue.c:2608) does the work; the doc says "Equivalent to queue_delayed_work_on() but tries to use the local CPU."
- **short-keyword** "virtual memory area find" → `find_vma` (grade 3): mm/mmap.c:903 (kerneldoc at 896) — "find_vma() - Find the VMA for a given address, or the next VMA." path: `find_vma` is also defined in mm/nommu.c:640. Grade 2 `vma_lookup` (include/linux/mm.h:4236, "vma_lookup() - Find a VMA at a specific address"; also defined in tools/testing/vma/include/dup.h).
- **short-keyword** "socket buffer clone" → `skb_clone` (grade 3): net/core/skbuff.c:2091 (kerneldoc at 2078) — "skb_clone	-	duplicate an sk_buff".
- **short-keyword** "workqueue alloc" → `alloc_workqueue` (grade 3): include/linux/workqueue.h:519 (kerneldoc at 482) — "alloc_workqueue - allocate a workqueue". Grade 2 `alloc_workqueue_noprof` (kernel/workqueue.c:5954) is the real function behind the alloc_hooks macro.
- **short-keyword** "dentry alloc" → `d_alloc` (grade 3): fs/dcache.c:1979 (kerneldoc at 1971) — "d_alloc - allocate a dcache entry". Grade 1 `d_alloc_parallel` (fs/dcache.c:2755) is the lookup-time variant.
- **short-keyword** "block io alloc bioset" → `bio_alloc_bioset` (grade 3): block/bio.c:535 (kerneldoc at 502/509) — "bio_alloc_bioset - allocate a bio for I/O" / "Allocate a bio from the mempools in @bs."

## subset (added 2026-09-03)

- **subset** "kmap local" → `kmap_local_page` (grade 3): include/linux/highmem-internal.h:71 — "static inline void *kmap_local_page(const struct page *page)"; kerneldoc at include/linux/highmem.h:63 — "kmap_local_page - Map a page for temporary usage". Grade 2 `kmap_local_folio` (highmem-internal.h:84) is the folio form. (Both have a second #ifdef-alternate definition in the same header, lines 186/196.)
- **subset** "rb insert" → `rb_insert_color` (grade 3): lib/rbtree.c:434 — "void rb_insert_color(struct rb_node *node, struct rb_root *root)"; Documentation/core-api/rbtree.rst:136-138 — "/* Add new node and rebalance tree. */ rb_link_node(&data->node, parent, new); rb_insert_color(&data->node, root);". path: also defined in tools/lib/rbtree.c:433. [path: `/lib/rbtree.c` — the name is defined in more than one file]
- **subset** "try to wake" → `try_to_wake_up` (grade 3): kernel/sched/core.c:4251 (kerneldoc at 4215) — "try_to_wake_up - wake up a thread".
- **subset** "generic file read" → `generic_file_read_iter` (grade 3): mm/filemap.c:2965 (kerneldoc at 2944) — "generic_file_read_iter - generic filesystem read routine".
- **subset** "kthread should" → `kthread_should_stop` (grade 3): kernel/kthread.c:148 (kerneldoc at 142) — "kthread_should_stop - should this kthread return now?" Grade 2 `kthread_should_park` (kernel/kthread.c:170) is the sibling predicate.
- **subset** "iget" → `iget_locked` (grade 3): fs/inode.c:1455 (kerneldoc at 1443) — "iget_locked - obtain an inode from a mounted file system". Grade 2 `iget5_locked` (fs/inode.c:1378, "a generalized version of iget_locked()"); grade 1 `ilookup` (fs/inode.c:1733, "search for an inode in the inode cache").
- **subset** "unmap mapping" → `unmap_mapping_range` (grade 3): mm/memory.c:4443 (kerneldoc at 4427) — "unmap_mapping_range - unmap the portion of all mmaps in the specified" [address_space corresponding to the specified byte range]. Grade 2 `unmap_mapping_pages` (mm/memory.c:4407, "unmap_mapping_pages() - Unmap pages from processes."). path: both also have static-inline !MMU stubs in include/linux/mm.h:3204/3206.

## descriptive (added 2026-09-03)

- **descriptive** "rcu callback after grace period" → `call_rcu` (grade 3): kernel/rcu/tree.c:3277 (kerneldoc at 3221) — "call_rcu() - Queue an RCU callback for invocation after a grace period." path: also defined in kernel/rcu/tiny.c:158 (and a macro in tools/testing/shared/linux/radix-tree.h).
- **descriptive** "allocate zeroed kernel memory" → `kzalloc` (grade 3): include/linux/slab.h:1301 — "#define kzalloc(size, flags)			alloc_hooks(kzalloc_noprof(size, flags))"; kerneldoc at 1295 — "kzalloc - allocate memory. The memory is set to zero." path: `kzalloc` is also defined in tools/include/linux/slab.h:138, tools/virtio/linux/kernel.h:74, tools/virtio/ringtest/ptr_ring.c:38. Grade 2 `kzalloc_noprof` (slab.h:1292) is the profiled inner macro. [path: `/include/linux/slab.h` — the name is defined in more than one file]
- **descriptive** "copy buffer out of userland" → `copy_from_user` (grade 3): include/linux/uaccess.h:218 — "copy_from_user(void *to, const void __user *from, unsigned long n)" (body 222: "return _copy_from_user(to, from, n);"); Documentation/kernel-hacking/hacking.rst:283-285 — "copy_to_user() and copy_from_user() are more general: they copy an arbitrary amount of data to and from userspace." path: also defined in tools/virtio/linux/uaccess.h:32. Grade 2 `_copy_from_user` (lib/usercopy.c:16) does the copy; path recorded because include/linux/uaccess.h:208 also `#define`s it as an alias under INLINE_COPY_USER.
- **descriptive** "remove node from red black tree" → `rb_erase` (grade 3): lib/rbtree.c:440 — "void rb_erase(struct rb_node *node, struct rb_root *root)"; Documentation/core-api/rbtree.rst:146-148 — "To remove an existing node from a tree, call:: void rb_erase(struct rb_node *victim, struct rb_root *tree);". path: also defined in tools/lib/rbtree.c:438. [path: `/lib/rbtree.c` — the name is defined in more than one file]
- **descriptive** "flag folio as modified" → `folio_mark_dirty` (grade 3): mm/page-writeback.c:2778 (kerneldoc at 2766) — "folio_mark_dirty - Mark a folio as being modified." Grade 2 `__folio_mark_dirty` (mm/page-writeback.c:2674, "Mark the folio dirty, and set it dirty in the page cache.").
- **descriptive** "cancel pending timer and wait for handler" → `timer_delete_sync` (grade 3): kernel/time/timer.c:1674 (kerneldoc at 1633) — "timer_delete_sync - Deactivate a timer and wait for the handler to finish." Grade 2 `timer_delete` (timer.c:1404, "The function only deactivates a pending timer, but contrary to timer_delete_sync() it does not take into account whether the timer's callback function is concurrently executed"; path: also defined in tools/include/nolibc/time.h:162).
- **descriptive** "resolve a string filename to a path struct" → `kern_path` (grade 3): fs/namei.c:3033 — "int kern_path(const char *name, unsigned int flags, struct path *path)", body 3035-3036: "CLASS(filename_kernel, filename)(name); return filename_lookup(AT_FDCWD, filename, flags, path, NULL);". Grade 2 `filename_lookup` (fs/namei.c:2834) does the resolution.
- **descriptive** "submit block io request" → `submit_bio` (grade 3): block/blk-core.c:952 (kerneldoc at 940/943) — "submit_bio - submit a bio to the block device layer for I/O" / "submit_bio() is used to submit I/O requests to block devices." Grade 2 `submit_bio_noacct` (blk-core.c:817, "re-submit a bio to the block device layer for I/O"). [path: `block/blk-core.c` — the name is defined in more than one file]
- **descriptive** "compute message digest in one shot" → `crypto_shash_digest` (grade 3): crypto/shash.c:183 — "int crypto_shash_digest(struct shash_desc *desc, const u8 *data,"; kerneldoc include/crypto/hash.h:884/890 — "crypto_shash_digest() - calculate message digest for buffer" / "This function is a \"short-hand\" for the function calls of crypto_shash_init, crypto_shash_update and crypto_shash_final." Grade 2 `crypto_shash_tfm_digest` (crypto/shash.c:196).
- **descriptive** "register a new device with the system" → `device_register` (grade 3): drivers/base/core.c:3851 (kerneldoc at 3834) — "device_register - register a device with the system." Grade 2 `device_add` (core.c:3639, "device_add - add device to device hierarchy." / "This is part 2 of device_register()"). [path: `drivers/base/core.c` — the name is defined in more than one file]
- **descriptive** "smoothed round trip time estimate" → `tcp_rtt_estimator` (grade 3): net/ipv4/tcp_input.c:1070 (comment at 1061) — "Called to compute a smoothed rtt estimate." Grade 1 `tcp_set_rto` (tcp_input.c:1175, "Calculate rto without backoff. This is the second half of Van Jacobson's routine referred to above.").
- **descriptive** "search parent dentry children for name" → `d_lookup` (grade 3): fs/dcache.c:2552 (kerneldoc at 2547) — "d_lookup searches the children of the parent dentry for the name in question." Grade 2 `__d_lookup` (fs/dcache.c:2582, "__d_lookup is like d_lookup, however it may (rarely) return a false-negative result").

## paraphrase (added 2026-09-03)

- **paraphrase** "place page array in contiguous kernel virtual range" → `vmap` (grade 3): mm/vmalloc.c:3537 (kerneldoc at 3529) — "Maps @count pages from @pages into contiguous kernel virtual space." path: also defined in mm/nommu.c:308. No token shared with `vmap`/vmalloc.c.
- **paraphrase** "defer a job to the system per-cpu pool" → `schedule_work` (grade 3): include/linux/workqueue.h:758 (kerneldoc at 751) — "This puts a job in the system per-CPU workqueue if it was not already queued". Grade 2 `queue_work` (workqueue.h:696) is what it calls (760: "return queue_work(system_percpu_wq, work);"). [path: `include/linux/workqueue.h` — the name is defined in more than one file] [path: `include/linux/workqueue.h` — the name is defined in more than one file]
- **paraphrase** "register attribute directory under kobject" → `sysfs_create_group` (grade 3): fs/sysfs/group.c:212 (kerneldoc at 203) — "sysfs_create_group - given a directory kobject, create an attribute group". path: a !SYSFS inline stub is also defined at include/linux/sysfs.h:625.
- **paraphrase** "block until all pre-existing readers finish" → `synchronize_rcu` (grade 3): kernel/rcu/tree.c:3378 (kerneldoc at 3343-3345) — "Control will return to the caller some time after a full grace period has elapsed, in other words after all currently executing RCU read-side critical sections have completed." path: also defined in kernel/rcu/tiny.c:141 and tools/.
- **paraphrase** "make a task runnable" → `wake_up_process` (grade 3): kernel/sched/core.c:4545 (kerneldoc at 4538) — "Attempt to wake up the nominated process and move it to the set of runnable processes." Grade 2 `try_to_wake_up` (core.c:4251) does the work (4547: "return try_to_wake_up(p, TASK_NORMAL, 0);").
- **paraphrase** "write out all cached changes of a mounted volume" → `sync_filesystem` (grade 3): fs/sync.c:30 (comment at 26-27) — "Write out and wait upon all dirty data associated with this superblock.  Filesystem data as well as the underlying block device." Grade 2 `sync_inodes_sb` (fs/fs-writeback.c:2993, "This function writes and waits on any dirty inode belonging to this super_block.").
- **paraphrase** "background page reclaim daemon thread" → `kswapd` (grade 3): mm/vmscan.c:7399 (comment at 7387) — "The background pageout daemon, started as a kernel thread from the init process." Grade 2 `kswapd_run` (vmscan.c:7593, "This kswapd start function will be called by init and node-hot-add."); grade 1 `balance_pgdat` (vmscan.c:7064, "For kswapd, balance_pgdat() will reclaim pages across a node"). [path: `mm/vmscan.c` — the name is defined in more than one file]
- **paraphrase** "pick victim task when memory exhausted" → `select_bad_process` (grade 3): mm/oom_kill.c:362 (comment at 359) — "Simple selection loop. We choose the process with the highest number of 'points'." Grade 2 `oom_badness` (oom_kill.c:199, "heuristic function to determine which candidate task to kill"); grade 1 `oom_kill_process` (oom_kill.c:1008).
- **paraphrase** "run function across all processors" → `on_each_cpu` (grade 3): include/linux/smp.h:70 (comment at 68) — "Call a function on all processors".

## conjunctive (added 2026-09-03)

- **conjunctive** "tcp sack reordering detection" → `tcp_check_sack_reordering` (grade 3): net/ipv4/tcp_input.c:1275 (comment at 1271-1272) — "It's reordering when higher sequence was delivered (i.e. sacked) before some lower never-retransmitted sequence (\"low_seq\")." Both concepts (SACK + reordering) visible; static in the giant tcp_input.c.
- **conjunctive** "expedited rcu grace period" → `synchronize_rcu_expedited` (grade 3): kernel/rcu/tree_exp.h:924 (kerneldoc at 905/907) — "synchronize_rcu_expedited - Brute-force RCU grace period" / "Wait for an RCU grace period, but expedite it." path: a tiny-RCU inline is also defined at include/linux/rcutiny.h:88. Grade 1 `synchronize_rcu` is the non-expedited form.
- **conjunctive** "killable rwsem write lock" → `down_write_killable` (grade 3): kernel/locking/rwsem.c:1639 (comment at 1637, body 1645-1648) — "lock for writing" / "if (LOCK_CONTENDED_RETURN(sem, __down_write_trylock, __down_write_killable)) { rwsem_release(&sem->dep_map, _RET_IP_); return -EINTR;". Grade 1 `down_write` (rwsem.c:1627; also defined in tools/perf/util/rwsem.c:51).
- **conjunctive** "synchronous readahead on cache miss" → `page_cache_sync_ra` (grade 3): mm/readahead.c:577 — "void page_cache_sync_ra(struct readahead_control *ractl," with body comment 609 — "A start of file, oversized read, or sequential cache miss:". Grade 2 `page_cache_sync_readahead` (include/linux/pagemap.h:1390, kerneldoc 1384: "page_cache_sync_readahead() should be called when a cache miss happened: it will submit the read.") is the inline wrapper that calls it (1395); grade 1 `page_cache_async_ra` (readahead.c:653) is the asynchronous sibling.
