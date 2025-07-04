# Changes

## [0.1.34] - 2025-06-25

* Make Handle type Send + Sync

## [0.1.33] - 2025-06-25

* Fix Debug impl for Handle

## [0.1.32] - 2025-06-25

* Refine Handle type

## [0.1.31] - 2025-06-11

* Fix submission sync order

## [0.1.30] - 2025-06-09

* Use new submission api for io-uring

## [0.1.27] - 2025-05-29

* Use inline ntex-io-uring api

## [0.1.25] - 2025-05-27

* Clear events list for poll driver

## [0.1.24] - 2025-05-27

* Detect kernel 6.1 or greater for io-uring config

## [0.1.23] - 2025-05-24

* Tune scheduler events handling

## [0.1.22] - 2025-05-19

* Use polling fork temporary

## [0.1.21] - 2025-05-13

* Handle EINTR error

## [0.1.20] - 2025-05-13

* Use io-uring fork temporary

## [0.1.19] - 2025-05-10

* Set pool worker thread name

* Fix remote wakers queue handling

## [0.1.18] - 2025-04-08

* Update io-uring Handler trait

## [0.1.17] - 2025-04-05

* Revert thread pool changes

## [0.1.16] - 2025-03-31

* Optimize runtime queue

## [0.1.15] - 2025-03-28

* Fix borrow panic in poll driver

## [0.1.14] - 2025-03-28

* io-uring driver cleanups

* Update polling dependency

## [0.1.13] - 2025-03-27

* io-uring driver cleanups

## [0.1.12] - 2025-03-26

* Various cleanups

## [0.1.10] - 2025-03-25

* Do not return error on timeout

## [0.1.9] - 2025-03-25

* Add old io-uring version compat

## [0.1.8] - 2025-03-25

* More polling driver simplifications

## [0.1.7] - 2025-03-21

* Simplify polling driver

## [0.1.6] - 2025-03-20

* Redesign polling driver

## [0.1.5] - 2025-03-17

* Add Probe::is_supported() api call for io-uring driver

## [0.1.4] - 2025-03-16

* Do not create fd item for unregister-all

## [0.1.3] - 2025-03-14

* Remove neon::net

## [0.1.2] - 2025-03-13

* Export runtime `Handle`

## [0.1.1] - 2025-03-13

* Simplify code

## [0.1.0] - 2025-03-12

* Initial impl
