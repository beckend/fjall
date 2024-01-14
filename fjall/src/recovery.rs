use crate::{
    batch::PartitionKey,
    file::{
        FLUSH_MARKER, FLUSH_PARTITIONS_LIST, JOURNALS_FOLDER, PARTITIONS_FOLDER,
        PARTITION_DELETED_MARKER,
    },
    journal::Journal,
    partition::PartitionHandleInner,
    Keyspace, PartitionHandle,
};
use lsm_tree::MemTable;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

/// Recovers partitions
pub fn recover_partitions(
    keyspace: &Keyspace,
    memtables: &mut HashMap<PartitionKey, MemTable>,
) -> crate::Result<()> {
    let partitions_folder = keyspace.config.path.join(PARTITIONS_FOLDER);

    for dirent in std::fs::read_dir(&partitions_folder)? {
        let dirent = dirent?;
        let partition_name = dirent.file_name();
        let partition_path = dirent.path();

        log::trace!("Recovering partition {:?}", partition_name);

        // IMPORTANT: Check deletion marker
        if partition_path.join(PARTITION_DELETED_MARKER).try_exists()? {
            log::debug!("Deleting deleted partition {:?}", partition_name);
            std::fs::remove_dir_all(partition_path)?;
            continue;
        }

        // Check for marker, maybe the partition is not fully initialized
        if !partition_path.join(".lsm").try_exists()? {
            log::debug!("Deleting uninitialized partition {:?}", partition_name);
            std::fs::remove_dir_all(partition_path)?;
            continue;
        }

        let partition_name = partition_name
            .to_str()
            .expect("should be valid partition name");

        let path = partitions_folder.join(partition_name);

        let tree = lsm_tree::Config::new(path)
            .descriptor_table(keyspace.config.descriptor_table.clone())
            .block_cache(keyspace.config.block_cache.clone())
            .open()?;

        let partition_inner = PartitionHandleInner {
            max_memtable_size: (8 * 1_024 * 1_024).into(),
            compaction_strategy: RwLock::new(Arc::new(lsm_tree::compaction::Levelled::default())),
            name: partition_name.into(),
            tree,
            partitions: keyspace.partitions.clone(),
            keyspace_config: keyspace.config.clone(),
            flush_manager: keyspace.flush_manager.clone(),
            flush_semaphore: keyspace.flush_semaphore.clone(),
            journal_manager: keyspace.journal_manager.clone(),
            journal: keyspace.journal.clone(),
            compaction_manager: keyspace.compaction_manager.clone(),
            seqno: keyspace.seqno.clone(),
            write_buffer_manager: keyspace.write_buffer_manager.clone(),
        };
        let partition_inner = Arc::new(partition_inner);
        let partition = PartitionHandle(partition_inner);

        // NOTE: We already recovered all active memtables from the active journal,
        // so just yank it out and give to the partition
        if let Some(recovered_memtable) = memtables.remove(partition_name) {
            log::trace!(
                "Recovered previously active memtable for {:?}, with size: {} B",
                partition_name,
                recovered_memtable.size()
            );

            // IMPORTANT: Add active memtable size to current write buffer size
            keyspace
                .write_buffer_manager
                .allocate(recovered_memtable.size().into());

            partition.tree.set_active_memtable(recovered_memtable);
        }

        // Recover seqno
        let maybe_next_seqno = partition.tree.get_lsn().map(|x| x + 1).unwrap_or_default();
        keyspace
            .seqno
            .fetch_max(maybe_next_seqno, std::sync::atomic::Ordering::AcqRel);

        log::debug!("Keyspace seqno is now {}", keyspace.seqno.get());

        // Add partition to dictionary
        keyspace
            .partitions
            .write()
            .expect("lock is poisoned")
            .insert(partition_name.into(), partition.clone());

        log::trace!("Recovered partition {:?}", partition_name);
    }

    Ok(())
}

pub fn recover_sealed_memtables(keyspace: &Keyspace) -> crate::Result<()> {
    let journals_folder = keyspace.config.path.join(JOURNALS_FOLDER);

    let mut dirents = std::fs::read_dir(journals_folder)?.collect::<std::io::Result<Vec<_>>>()?;
    dirents.sort_by_key(std::fs::DirEntry::file_name);

    log::debug!("Recovering sealed journals: {dirents:#?}");

    for dirent in dirents {
        let journal_path = dirent.path();

        // Check if journal is sealed
        if dirent.path().join(FLUSH_MARKER).try_exists()? {
            let journal_size = fs_extra::dir::get_size(&journal_path).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("{:?}", e.kind))
            })?;

            log::trace!("Reading sealed journal at {:?}", journal_path);

            // Only consider partitions that are registered in the journal
            let partitions_to_consider =
                std::fs::read_to_string(journal_path.join(FLUSH_PARTITIONS_LIST))?;

            let partitions_to_consider = partitions_to_consider
                .split('\n')
                .filter(|x| !x.is_empty())
                .map(|x| {
                    let mut splits = x.split(':');
                    let name = splits.next().expect("partition name should exist");
                    let lsn = splits.next().expect("lsn should exist");
                    let lsn = lsn
                        .parse::<lsm_tree::SeqNo>()
                        .expect("should be valid seqno");

                    (name, lsn)
                })
                .collect::<Vec<_>>();

            log::trace!(
                "Journal contains data of {} partitions",
                partitions_to_consider.len()
            );

            let mut partition_seqno_map = HashMap::default();
            let partitions_lock = keyspace.partitions.read().expect("lock is poisoned");

            // Only get the partitions that have a lower seqno than the journal
            // which means there's still some unflushed data in this sealed journal
            for (partition_name, lsn) in partitions_to_consider {
                let Some(partition) = partitions_lock.get(partition_name) else {
                    // Partition was probably deleted
                    log::trace!("Partition {partition_name:?} does not exist");
                    continue;
                };

                let partition_lsn = partition.tree.get_segment_lsn();
                let has_lower_lsn = partition_lsn.map_or(true, |partition_lsn| lsn > partition_lsn);

                if has_lower_lsn {
                    partition_seqno_map.insert(
                        partition_name.into(),
                        crate::journal::manager::PartitionSeqNo {
                            lsn,
                            partition: partition.clone(),
                        },
                    );
                } else {
                    log::trace!("Partition {partition_name:?} has higher seqno, skipping");
                }
            }

            let mut journal_manager_lock =
                keyspace.journal_manager.write().expect("lock is poisoned");

            let mut flush_manager_lock = keyspace.flush_manager.write().expect("lock is poisoned");

            // Recover sealed memtables for affected partitions
            let partition_names_to_recover =
                partition_seqno_map.keys().cloned().collect::<Vec<_>>();

            log::trace!("Recovering memtables for partitions: {partition_names_to_recover:#?}");
            let memtables = Journal::recover_memtables(
                &journal_path,
                Some(&partition_names_to_recover),
                keyspace.config.journal_recovery_mode,
            )?;
            log::trace!("Recovered {} sealed memtables", memtables.len());

            // IMPORTANT: Add sealed journal to journal manager
            journal_manager_lock.enqueue(crate::journal::manager::Item {
                partition_seqnos: partition_seqno_map,
                path: journal_path.clone(),
                size_in_bytes: journal_size,
            });

            // Consume memtables by giving back to the partition
            for (partition_name, sealed_memtable) in memtables {
                let Some(partition) = partitions_lock.get(&partition_name) else {
                    // Should not happen
                    continue;
                };

                let memtable_id = lsm_tree::id::generate_segment_id();
                let sealed_memtable = Arc::new(sealed_memtable);

                partition
                    .tree
                    .add_sealed_memtable(memtable_id.clone(), sealed_memtable.clone());

                // Maybe the memtable has a higher seqno, so try to set to maximum
                let maybe_next_seqno = partition.tree.get_lsn().map(|x| x + 1).unwrap_or_default();
                keyspace
                    .seqno
                    .fetch_max(maybe_next_seqno, std::sync::atomic::Ordering::AcqRel);

                log::debug!("Keyspace seqno is now {}", keyspace.seqno.get());

                // IMPORTANT: Add sealed memtable size to current write buffer size
                keyspace
                    .write_buffer_manager
                    .allocate(sealed_memtable.size().into());

                // TODO: unit test write buffer size after recovery

                // IMPORTANT: Add sealed memtable to flush manager, so it can be flushed
                flush_manager_lock.enqueue_task(
                    partition_name,
                    crate::flush::manager::Task {
                        id: memtable_id,
                        sealed_memtable,
                        partition: partition.clone(),
                    },
                );
            }

            log::trace!("Requeued sealed journal at {:?}", journal_path);
        }
    }

    Ok(())
}
