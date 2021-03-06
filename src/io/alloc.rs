//! Page management.
//!
//! Pages are virtual data units of size 4088 bytes. They're represented on disk somewhat
//! non-obviously, since clusters can hold more than one page at once (compression). Every cluster
//! will maximize the number of pages held and when it's filled up, a new cluster will be fetched.

/// The atomic ordering used in the allocator.
const ORDERING: atomic::Ordering = atomic::Ordering::Relaxed;

quick_error! {
    /// A page management error.
    enum Error {
        /// No clusters left in the freelist.
        ///
        /// This is the equivalent to OOM, but with disk space.
        OutOfClusters {
            description("Out of free clusters.")
        }
        /// A page checksum did not match.
        ///
        /// The checksum of the data and the checksum stored in the page pointer did not match.
        ///
        /// This indicates some form of data corruption in the sector storing the page.
        PageChecksumMismatch {
            /// The page with the mismatching checksum.
            page: page::Pointer,
            /// The actual checksum of the page.
            found: u32,
        } {
            display("Mismatching checksums in {} - expected {:x}, found {:x}.",
                    page, page.checksum, found)
            description("Mismatching checksum in page.")
        }
        /// A metacluster checksum did not match.
        ///
        /// The checksum of the metacluster and the checksum stored in the previous metacluster
        /// pointer did not match.
        ///
        /// This indicates some form of data corruption in the sector storing the metacluster.
        MetacluterChecksumMismatch {
            /// The corrupted metacluster whose stored and actual checksum mismatches.
            cluster: cluster::Pointer,
            /// The expected/stored checksum.
            expected: u64,
            /// The actual checksum of the data.
            found: u64,
        } {
            display("Mismatching checksums in metacluster {:x} - expected {:x}, found {:x}.",
                    cluster, expected.checksum, found)
            description("Mismatching checksum in metacluster.")
        }
        /// The compressed data is invalid and cannot be decompressed.
        ///
        /// Multiple reasons exists for this to happen:
        ///
        /// 1. The compression configuration option has been changed without recompressing clusters.
        /// 2. Silent data corruption occured, and did the unlikely thing to has the right checksum.
        /// 3. There is a bug in compression or decompression.
        InvalidCompression {
            cluster: cluster::Pointer,
        } {
            display("Unable to decompress data from cluster {}.", cluster)
            description("Unable to decompress data.")
        }
        /// A disk error.
        Disk(err: disk::Error) {
            from()
            description("Disk I/O error.")
            display("Disk I/O error: {}", err)
        }
    }
}

/// The state of some cluster.
///
/// This caches a cluster uncompressed such that there is no need for decompression when appending
/// a new page into the cluster.
struct ClusterState {
    /// The pointer to the cluster.
    cluster: cluster::Pointer,
    /// The cluster uncompressed.
    ///
    /// This is used for packing pages into the cluster, by appending the new page to this vector
    /// and then compressing it to see if it fits into the cluster. If it fails to fit, the vector
    /// is reset and a new cluster is allocated.
    uncompressed: Vec<u8>,
}

/// A metacluster.
///
/// Metaclusters points to other free clusters, and possibly a metacluster. Metacluters can be seen
/// as nodes of the unrolled linked list of free blocks.
struct Metacluster {
    /// Checksum of the next metacluster.
    next_checksum: u64,
    /// Pointer to the next metacluster.
    next: Option<cluster::Pointer>,
    /// Pointers to free clusters.
    free: Vec<cluster::Pointer>,
}

impl Metacluster {
    /// Encode the metacluster.
    ///
    /// This encodes the metacluster into its binary representation.
    fn encode(&self) -> [u8; disk::SECTOR_SIZE] {
        // Start with an all-null buffer.
        let mut buf = [0; disk::SECTOR_SIZE];

        // Write the checksum of the next metacluster.
        LittleEndian::write(&mut buf, self.next_checksum);
        // Write the pointer to the next metacluster.
        LittleEndian::write(&mut buf[8..], self.next.map_or(0, |x| x.into()));

        // Write every pointer of the freelist into the buffer.
        for (n, i) in self.head_metacluster.free.iter().enumerate() {
            LittleEndian::write(&mut buf[cluster::POINTER_SIZE * i + 8..], i);
        }

        buf
    }

    /// Calculate the checksum of this metacluster.
    ///
    /// This calculates the checksum of the non-empty part of its serialization with algorithm
    /// `algorithm`.
    fn checksum(&self, algorithm: header::ChecksumAlgorithm) -> u64 {
        // Only hash the initialized/active part of the metacluster.
        algorithm.hash(self.encode()[..(self.free + 1) * cluster::POINTER_SIZE + 8])
    }
}

/// The page manager.
///
/// This is the center point of the I/O stack, providing allocation, deallocation, compression,
/// etc. It manages the clusters (with the page abstraction) and caches the disks.
struct Manager {
    /// The inner disk cache.
    cache: Cache,
    /// The on-disk state.
    ///
    /// This is the state as stored in the state block. The reason we do not store the whole state
    /// block in one is that, we want to avoid the lock when reading the static parts of the state
    /// block (e.g. configuration).
    state: Mutex<state_block::State>,
    /// The configuration options.
    ///
    /// This is the configuration part of the state block. We don't need a lock, since we won't
    /// mutate it while the system is initialized.
    config: state_block::Config,
    /// The first metacluster of the freelist.
    ///
    /// This list is used as the allocation primitive of TFS. It is a simple freelist-based cluster
    /// allocation system, but there is one twist: To optimize the data locality, the list is
    /// unrolled.
    head_metacluster: Mutex<Metacluster>,
    /// The last allocated cluster.
    ///
    /// If possible, newly allocated pages will be appended to this cluster. When it is filled
    /// (i.e. the pages cannot compress to the cluster size or less), a new cluster will be
    /// allocated.
    last_cluster: AtomicOption<ClusterState>,
    /// The deduplication table.
    ///
    /// This table allows the allocator for searching for candidates to use instead of allocating a
    /// new cluster. In particular, it searches for duplicates of the allocated page.
    dedup_table: dedup::Table,
}

impl Manager {
    /// Open the manager from some driver.
    ///
    /// This loads the state page and other things from a vdev driver `driver`. If it fails, an
    /// error is returned.
    fn open(driver: vdev::Driver) -> Result<Manager, Error> {
        unimplemented!();
    }

    /// Allocate a page.
    ///
    /// This allocates a page with content `buf`.
    ///
    /// The algorithm works greedily by fitting as many pages as possible into the most recently
    /// used cluster.
    pub fn alloc(&mut self, buf: disk::SectorBuf) -> Result<cache::Transacting<page::Pointer>, Error> {
        // TODO: The variables are named things like `ptr`, which kinda contradicts the style of
        //       the rest of the code.

        /// The capacity (in bytes) of a compressed cluster.
        ///
        /// This is the maximal number of bytes that a cluster can contain decompressed.
        const CLUSTER_CAPACITY: usize = 512 * 2048;

        // Calculate the checksum of the buffer. We'll use this later.
        let cksum = self.checksum(buf) as u32;
        debug!(self, "allocating page"; "checksum" => cksum);

        // Check if duplicate exists.
        if let Some(page) = self.dedup_table.dedup(buf, cksum) {
            debug!(self, "found duplicate page"; "page" => page);
            // Deduplicate and simply use the already stored page. No transaction where required.
            return Ok(cache::Transacting::no_transaction(page));
        }

        // Handle the case where compression is disabled.
        if self.config.compression_algorithm == CompressionAlgorithm::Identity {
            // Pop a cluster from the freelist.
            let cluster = self.freelist_pop()?;

            let ptr = page::Pointer {
                cluster: cluster,
                offset: None,
                checksum: cksum,
            };

            // Insert the page pointer into the deduplication table to allow future use as
            // duplicate.
            self.dedup_table.insert(buf, ptr);

            // Write the cluster with the raw, uncompressed data, and return the transaction monad.
            return Ok(cluster.then(self.cache.write(cluster, buf)).wrap(ptr));
        }

        if let Some(state) = self.last_cluster.take(ORDERING) {
            // We have earlier allocated a cluster, meaning that we can potentially append more
            // pages into the cluster.

            // Check if the capacity of the cluster is exceeded. If so, jump out of the `if`, and
            // allocate a new cluster. This limit exists to avoid unbounded memory use which can be
            // exploited by a malicious party to force an OOM crash.
            if state.uncompressed.len() < CLUSTER_CAPACITY {
                trace!(self, "extending existing cluster";
                       "old length" => state.uncompressed.len());

                // Extend the buffer of uncompressed data in the last allocated cluster.
                state.uncompressed.extend_from_slice(buf);

                // Check if we can compress the extended buffer into a single cluster.
                if let Some(compressed) = self.compress(state.uncompressed) {
                    // Put back the "last cluster", as it might be possible to fit in even more
                    // pages later on.
                    self.last_cluster.swap(Some(state), ORDERING);

                    let ptr = Ok(page::Pointer {
                        cluster: state.cluster,
                        // Calculate the offset into the decompressed buffer, where the page is
                        // stored.
                        offset: Some(state.uncompressed / disk::SECTOR_SIZE - 1),
                        checksum: cksum,
                    });

                    // Insert the page pointer into the deduplication table to allow future use as
                    // duplicate.
                    self.dedup_table.insert(buf, ptr);

                    // It succeeded! Write the compressed data into the cluster. Wrap the pointer
                    // in the transaction and return it.
                    return self.cache.write(state.cluster, compressed).wrap(ptr);
                }
            }
        }

        // We were unable to extend the last allocated cluster, either because there is no last
        // allocated cluster, or because the cluster could not contain the page. We'll allocate a
        // new cluster to contain our page.

        // Pop the cluster from the freelist.
        let cluster = self.freelist_pop()?;
        let ptr = if let Some(compressed) = self.compress(buf) {
            trace!(self, "storing compressible page in cluster"; "cluster" => cluster);

            // We were able to compress the page to fit into the cluster. At first, compressing the
            // first page seems unnecessary as it is guaranteed to fit in without compression, but
            // it has a purpose: namely that it allows us to extend the cluster. Enabling
            // compression in an uncompressed cluster is not plausible, as it would require
            // updating pointers pointing to the clujster. However, when we are already compressed,
            // there is no change in how the other pages are read.

            // Make the "last cluster" the newly allocated cluster.
            self.last_cluster.swap(Some(ClusterState {
                cluster: cluster,
                // So far, it only contains one page.
                uncompressed: buf.as_vec(),
            }), ORDERING);

            // Write the compressed data into the cluster.
            cluster.then(self.cache.write(cluster, compressed)).wrap(page::Pointer {
                cluster: cluster,
                offset: Some(0),
                checksum: cksum,
            })
        } else {
            trace!(self, "storing incompressible page in cluster"; "cluster" => cluster);

            // We were not able to compress the page into a single cluster. We work under the
            // assumption, that we cannot do so either when new data is added. This makes the
            // algorithm greedy, but it is a fairly reasonable assumption to make, as most
            // compression algorithm works at a stream level, and even those that don't (e.g.
            // algorithms with a reordering step), rarely shrinks by adding more data.

            // `self.last_cluster` will continue being `None`, until an actually extendible
            // (compressed) cluster comes in.

            // Write the data into the cluster, uncompressed.
            cluster.then(self.cache.write(cluster, buf)).replace_inner(page::Pointer {
                cluster: cluster,
                offset: None,
                checksum: cksum,
            })
        };

        // Insert the page pointer into the deduplication table to allow future use as
        // duplicate.
        self.dedup_table.insert(buf, ptr);

        Ok(ptr)
    }

    /// Read/dereference a page.
    ///
    /// This reads page `page` and returns the content.
    pub fn read(&self, page: page::Pointer) -> Result<disk::SectorBuf, Error> {
        trace!(self, "reading page"; "page" => page);

        // Read the cluster in which the page is stored.
        self.cache.read_then(page.cluster, |cluster| {
            // Decompress if necessary.
            let buf = if let Some(offset) = page.offset {
                // The page is compressed, decompress it and read at some offset `offset` (in pages).

                // Decompress the cluster.
                let decompressed = self.decompress(cluster)?;

                // Read the decompressed stream from some offset, into a sector buffer.
                let mut tmp = disk::SectorBuf::default();
                // TODO: Find a way to eliminate this memcpy.
                tmp.copy_from_slice(decompressed[offset * disk::SECTOR_SIZE..][..disk::SECTOR_SIZE]);

                tmp
            } else {
                // The page was not compressed so we can just use the cluster directly.
                cluster
            };

            // Check the data against the stored checksum.
            let cksum = self.checksum(buf) as u32;
            if cksum as u32 != page.checksum {
                // The checksums mismatched, thrown an error.
                return Err(Error::PageChecksumMismatch {
                    page: page,
                    found: cksum,
                });
            }

            Ok(ret)
        })
    }

    /// Calculate the checksum of some buffer, based on the user configuration.
    fn checksum(&self, buf: &[u8]) -> u64 {
        trace!(self, "calculating checksum");

        self.driver.header.hash(buf)
    }

    /// Compress some data based on the compression configuration option.
    ///
    /// # Panics
    ///
    /// This will panic if compression is disabled.
    fn compress(&self, input: &[u8]) -> Option<disk::SectorBuf> {
        trace!(self, "compressing data");

        // Compress the input.
        let compressed = match self.config.compression_algorithm {
            // We'll panic if compression is disabled, as it is assumed that the caller handles
            // this case.
            CompressionAlgorithm::Identity => panic!("Compression was disabled."),
            // Compress via LZ4.
            CompressionAlgorithm::Lz4 => lz4_compress::compress(input),
        };

        if compressed.len() < disk::SECTOR_SIZE {
            // We were able to compress the input into at least one cluster. Now, we apply padding.

            // Write a delimiter to make the padding distinguishable from the actual data (e.g. if
            // it ends in zero).
            // TODO: This is not bijective. Very bad! FAKE NEWS
            compressed.push(0xFF);

            // Convert it to type `disk::SectorBuf`.
            let mut buf = disk::SectorBuf::default();
            // TODO: Find a way to eliminate this memcpy.
            buf[..compressed.len()].copy_from_slice(&compressed);
        } else {
            // We were unable to compress the input into one cluster.
            None
        }
    }

    /// Decompress some data based on the compression configuration option.
    ///
    /// # Panics
    ///
    /// This will panic if compression is disabled.
    fn decompress(&self, cluster: disk::SectorBuf) -> Result<Box<[u8]>, Error> {
        trace!(self, "decompressing data");

        // Find the padding delimited (i.e. the last non-zero byte).
        if let Some((len, _)) = cluster.enumerate().rev().find(|(_, x)| x != 0) {
            // We found the delimiter and can now distinguish padding from data.
            Ok(match self.config.compression_algorithm {
                // We'll panic if compression is disabled, as it is assumed that the caller handles
                // this case.
                CompressionAlgorithm::Identity => panic!("Compression was disabled."),
                // Decompress the non-padding section from LZ4.
                CompressionAlgorithm::Lz4 => lz4_compress::decompress(source[..len])?,
            })
        } else {
            // No delimiter was found, indicating data corruption.
            // TODO: Use a special error for this.
            Err(Error::InvalidCompression)
        }
    }

    /// Flush the state block.
    ///
    /// This flushes the state block (not to the disk, but to the cache), and returns the
    /// transaction.
    ///
    /// It takes a state in order to avoid re-acquiring the lock.
    fn flush_state_block(&mut self, state: &state_block::State) -> cache::Transaction {
        trace!(self, "flushing the state block to the cache");

        // Do it, motherfucker.
        self.cache.write(self.driver.header.state_block_address, state_block::StateBlock {
            config: self.config,
            state: state,
        }.encode())
    }

    /// Write the head metacluster to some cluster.
    ///
    /// The cache transaction is returned.
    fn write_head_metacluster(&mut self, cluster: cluster::Pointer) -> cache::Transaction {
        trace!(self, "writing the head metacluster"; "target cluster" => cluster);

        self.cache.write(cluster, self.head_metacluster.encode());
    }

    /// Pop from the freelist.
    ///
    /// The returned pointer is wrapped in a cache transaction, representing the operations done in
    /// order to pop it.
    ///
    /// The algorithm works as follows: If the head metacluster contains more free clusters, simply
    /// pop and return the pointer. If not, make the next metacluster the head metacluster and
    /// return the old metacluster.
    fn freelist_pop(&mut self) -> Result<cache::Transacting<cluster::Pointer>, Error> {
        trace!(self, "popping from freelist");

        // Lock the state.
        let state = self.state.lock();

        if let Some(freelist_head) = state.freelist_head.take() {
            if let Some(free) = self.head_metacluster.free.pop() {
                // There were one or more free clusters in the head metacluster, we pop the last
                // free cluster in the metacluster.

                // Decrement the cluster counter to "truncate" the metacluster. This trick saves us
                // from passing through an inconsistent state as we can update the checksum and the
                // counter in the same sector write.
                freelist_head.counter -= 1;
                // Update the checksum to reflect the change made to the metacluster.
                freelist_head.checksum = self.head_metacluster.checksum();

                // Put back the freelist head into the state block.
                state.freelist_head = freelist_head;

                // Flush the state block to reflect the changes above. Because both the checksum
                // and counter are updated, this will be atomic and consistent. Wrap the output in
                // the transaction.
                Ok(self.flush_state_block(&state).wrap(free))
            } else {
                // There were no free clusters, but there might be additional metaclusters. The
                // outline of the algorithm is to update the freelist head pointer to point to the
                // next metacluster, if any, and then use the current, exhausted metacluster as the
                // allocated cluster.

                // The head metacluster is now empty, update the head to the next metacluster, if
                // it exist.
                let transaction = if let Some(next_metacluster) = self.head_metacluster.next_metacluster.take() {
                    // A new metacluster existed.
                    debug!(self, "switching metacluster"; "new metacluster" => next_metacluster);

                    // Read and decode the metacluster.
                    if let Ok(metacluster) = self.cache.read_then(next_metacluster.into()?, |buf| {
                        // Decode the new metacluster.
                        let metacluter = Metacluster::decode(buf);
                        // Calculate the checksum.
                        // TODO: This can be done much more efficiently, as we already have the
                        //       decoded buffer. No need for re-decoding it.
                        let checksum = metacluster.checksum();

                        // Check the metacluster against the checksum stored in the older block.
                        if checksum != self.head_metacluster.next_checksum {
                            // Everything suceeded.
                            Ok(metacluster)
                        } else {
                            // Checksum mismatched; throw an error.
                            Err(Error::ChecksumMismatch {
                                cluster: next_metacluster,
                                // This was the stored checksum.
                                expected: self.head_metacluster.next_checksum,
                                // And the actual checksum.
                                found: checksum,
                            })
                        }
                    }) {
                        // Update the head metacluster to the decoded cluster.
                        self.head_metacluster = metacluster;
                        // Update the state block with the data from the newly decoded metacluster.
                        state.freelist_head = Some(state_block::FreelistHead {
                            // The pointer should point towards the new metacluster.
                            cluster: next_metacluster,
                            checksum: checksum,
                            // Since the cluster can at most contain 63 < 256 clusters, casting to u8
                            // won't cause overflow.
                            counter: self.head_metacluster.free.len() as u8,
                        });

                        // We flush the state block flush to write down our changes to the state block.
                        Some(self.flush_state_block(&state))
                    } else { None }
                } else { None };

                // Use _the old_ head metacluster as the allocated cluster, and wrap it in the
                // potential transaction from updating the metacluster head.
                Ok(cache::Transacting::new(freelist_head.cluster, transaction))
            }
        } else {
            // There is no freelist head, rendering the freelist empty, hence there is no cluster
            // to allocate. Return an error.
            Err(Error::OutOfClusters)
        }
    }

    /// Push to the freelist.
    ///
    /// This pushes `cluster` to the freelist and returns the cache transaction, or an error.
    ///
    /// The algorithm works as follows: If the metacluster is full, the pushed cluster is used as
    /// the new, empty head metacluster, which is linked to the old head metacluster. If not, the
    /// free cluster is simply pushed.
    fn freelist_push(&mut self, cluster: cluster::Pointer) -> cache::Transaction {
        trace!(self, "pushing to freelist"; "cluster" => cluster);

        // Lock the state.
        let state = self.state.lock();

        if let Some(freelist_head) = state.freelist_head {
            if self.head_metacluster.free.len() + 2 == disk::SECTOR_SIZE / cluster::POINTER_SIZE {
                // The head metacluster is full, so we will use the cluster to create a new
                // head metacluster.
                debug!(self, "creating new metacluster"; "cluster" => cluster);

                // Clear the free clusters to make ensure that there isn't duplicates.
                self.head_metacluster.free.clear();
                // Update the head metacluster's next pointer to point to the old head metacluster.
                self.head_metacluster.next = Some(freelist.cluster);
                // Update the head metacluster's next metacluster checksum to be the checksum of
                // the old metacluster as stored in the state block, since the old metacluster will
                // become the new metacluster's next. This simple trick is allows us to bypass
                // recalculation of the checksum. Small optimization, but hey, it works.
                self.head_metacluster.next_checksum = freelist_head.next_checksum;
                // Update the state block freelist head metadata to point to the new head
                // metacluster.
                state.freelist_head = Some(state_block::FreelistHead {
                    cluster: cluster,
                    // Calculate the checksum of the new head metacluster.
                    checksum: self.head_metacluster.checksum(),
                    // Currently, no free clusters are stored in the new head metacluster, so the
                    // counter is 0.
                    counter: 0,
                });
                // Write the metacluster to `cluster`. This won't leave the system in an
                // inconsistent state, as only `cluster`, which is free, will be changed.
                self.write_head_metacluster(cluster).then(
                    // Flush the state block. This won't leave the system in an inconsistent state
                    // either, as a new, valid metacluster is stored at `cluster`.
                    self.flush_state_block(&state)
                )
            } else {
                // There is more space in the head metacluster.

                // Push the new free cluster.
                self.head_metacluster.free.push(cluster);
                // Flush. Woosh!
                self.flush_state_block(&state)
            }
        } else {
            // The freelist is empty, so we set the cluster up as an empty metacluster as the
            // head metacluster.
            state.freelist_head = Some(state_block::FreelistHead {
                cluster: cluster,
                checksum: 0,
                counter: 0,
            });
            // Flush the state block to add the new cluster.
            self.flush_state_block(&state)
        }
    }
}

delegate_log!(Manager.cache);
