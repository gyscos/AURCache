# Summary

The plan is to move dependency resolution up, from paru running in the builder container, into AURCache itself.

# Library

Use the `aur-client` library from ~/fetched/aur-client/ for the following actions:
* Resolve the base package of a given package name.
* List the dependencies of a base package.
* Prepare the tarball URL for a base package source.

# Database changes

* Store base packages.
* Each package has a list of dependencies for other base packages.
    * Each such dependency can be tagged with a list of platforms for which it applies.
    * Storing dependencies could be done in a dependencies table (with, among others, a "dependent" and a "dependee" columns? unless there are more natural names for these).
* Each package also has a flag indicating whether it's directly requested. If not, it means it's present as a dependency only.
* Each package stores the "current version" (not necessarily built yet).

* When adding a new package, AURCache:
    * Looks up the base package for the given name
    * If this base package was already added, exit early.
    * Look at all the dependencies (including make depends) for this base package.
    * For any dependency not in the core/extra/multilib repos, add this package recursively.
    * Add the base package to the DB, with the dependencies to the added AUR packages.

# Building

* To build a package, we need all its dependencies to be built at the required versions.
* The builder is updated to include AURCache itself as a repository under the name `repo`.
* To build the package, the builder is given a link to the source tarball. It just needs to download & extract it, then run `makepkg -s`. This will pull in dependencies from the official and aurcache repos.

# Cleaning up

* "Live checking" can be done on a package. It means:
  * If the package is directly installed, return early.
  * If not:
      * Look for any other package that depends on this one. If any exist, return early.
      * If not:
        * Remove this package
        * Live check all of this package dependencies

* When a package is updated, if the set of dependencies changed, live-check any dependency that is no longer present in the new package dependencies.
* A possible user action is to "remove a package". This removes the "directly requested" flag from that package, then live-check it.
