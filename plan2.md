# Summary

The goal is to move from on-demand builder containers being spawned by AURCache, to persistent builders requesting build jobs from AURCache.
The builders could either be defined in the same docker-compose as AURCache, or running outside of docker as systemd service, potentially on different machines.

## Build Job API

A new API will be added. It will not be authenticated unlike the regular API.
* Register a new builder with some basic info
  * The builder could include its configured job parallelism, cpu and memory available, host platform
  * Returns maybe a UUID to identify the builder
  * Also tell the builder the name of the repo (it's usually `repo`).
  * Maybe tell the builder about the custom pacman.conf/makepkg.conf.
* Request a job
  * Long poll until a job is actually available.
  * Maybe download the PKGBUILD snapshot here.
  * The request lists the available platforms.
    Give in higher priority the native platform of the builder, then other supported platforms.
* Report build result
  * Success/failure
  * On success, report the name of the new archive files.

## builder

A new long-running builder service should be created.
* Intended to have a target folder mounted in a specific place (either NFS, docker volume, ...)
* Configurable with either a config file or env variables.
  * AURCache instance IP/hostname
    * Port for the jobs API
    * Port For the pacman repository itself
  * Workdir where PKGBUILDs are stored
  * Platforms to build (defaults to only the host platform)
  * Concurrent builds
  * Target folder where packages should be moved
  * Optionally a name for the builder?
* When starting, the builder:
  * Registers itself to the configured AURCache.
  * Prepares a custom pacman.conf with the configured aurcache repository.
  * Prepares a chroot with mkarchchroot, using the custom pacman.conf with `-C`. Install some base packages in there (base-devel, multilib-devel).
* It then starts polling for build jobs.
* When a build job is available:
  * Download the snapshot with the PKGBUILD in a sub-folder of the workdir (named by the package to build)
  * List the gpg keys from the SRCINFO, import them in the chroot if needed.
  * Uses makechrootpkg to build the package in a chroot with `-l $pkgname` so each package use a different chroot.
  * When complete, report the status to AURCache:
    * Send the full stdout/stderr
    * If build succeeds, move the packages to the target folder, then report the package names to AURCache.


## Open questions

* Should the job API use the same port as the regular API? Or a different one?
  * That API will not be authenticated, or at least not the same way
  * A different port makes it easier to isolate from external access and only open it locally
* Should we protect against malicious builders?
  * Should AURCache explicitly allow some builders?
  * Or require some crypto signature of the builder by some key?
* Should we protect the communication with the builder? Against a malicious agent in the same network.
  * Use HTTPS?
  * Authenticate the builder after they register?
* A solution to that may be to require the api port to be on a secure network (either local docker-compose network or secure VPN).
  * Maybe we should then allow configuring AURCache to only listen on a specific VPN address?
