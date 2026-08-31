import 'package:aurcache/api/packages.dart';
import 'package:aurcache/models/extended_package.dart';
import 'package:aurcache/providers/builds.dart';
import 'package:aurcache/providers/packages.dart';
import 'package:flutter/material.dart';
import 'package:flutter_riverpod/flutter_riverpod.dart';
import 'package:flutter_tags_x/flutter_tags_x.dart';
import 'package:go_router/go_router.dart';
import 'package:url_launcher/url_launcher.dart';

import '../api/API.dart';
import '../components/api/api_builder.dart';
import '../components/builds_table.dart';
import '../components/confirm_popup.dart';
import '../components/package_source_patch_popup.dart';
import '../constants/color_constants.dart';
import '../models/build.dart';
import '../providers/activity_log.dart';
import '../utils/file_formatter.dart';
import '../providers/statistics.dart';

class PackageScreen extends ConsumerStatefulWidget {
  const PackageScreen({super.key, required this.pkgbase});

  final String pkgbase;

  @override
  ConsumerState<PackageScreen> createState() => _PackageScreenState();
}

class _PackageScreenState extends ConsumerState<PackageScreen> {
  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(),
      body: APIBuilder(
        interval: Duration(minutes: 1),
        onLoad: () => _build(ExtendedPackage.dummy()),
        onData: (ExtendedPackage pkg) => _build(pkg),
        provider: getPackageProvider(widget.pkgbase),
      ),
    );
  }

  Widget _build(ExtendedPackage pkg) {
    return Padding(
      padding: const EdgeInsets.all(defaultPadding),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            mainAxisAlignment: MainAxisAlignment.spaceBetween,
            crossAxisAlignment: CrossAxisAlignment.center,
            children: [
              Row(
                children: [
                  Container(
                    margin: const EdgeInsets.only(left: 15),
                    child: Row(
                      children: [
                        Text(pkg.name, style: const TextStyle(fontSize: 32)),
                        if (!pkg.directly_requested) ...[
                          const SizedBox(width: 10),
                          Text(
                            "(dependency)",
                            style: TextStyle(
                              fontSize: 18,
                              color: Colors.white.withValues(alpha: 0.7),
                            ),
                          ),
                        ],
                      ],
                    ),
                  ),
                  pkg.package_source.maybeWhen(
                    aur: (aur) => IconButton(
                      onPressed: () async {
                        await launchUrl(
                          Uri.parse(aur.aur_url),
                          webOnlyWindowName: '_blank',
                        );
                      },
                      icon: const Icon(Icons.link),
                    ),
                    git: (git) => IconButton(
                      onPressed: () async {
                        await launchUrl(
                          Uri.parse(git.url),
                          webOnlyWindowName: '_blank',
                        );
                      },
                      icon: const Icon(Icons.link),
                    ),
                    orElse: () => const SizedBox.shrink(),
                  ),
                ],
              ),
              _buildTopActionButtons(pkg),
            ],
          ),
          Expanded(
            child: Row(
              mainAxisAlignment: MainAxisAlignment.spaceBetween,
              crossAxisAlignment: CrossAxisAlignment.start,
              children: [
                Expanded(child: _buildMainBody(pkg)),
                _buildSideBar(pkg),
              ],
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildTopActionButtons(ExtendedPackage pkg) {
    return Row(
      children: [
        ElevatedButton(
          onPressed: () async {
            await showConfirmationDialog(
              context,
              "Force Rebuild Package",
              "Are you sure to force an Package rebuild?\nIf the Package is outdated, the newest version is built.",
              () async {
                await API.updatePackage(force: true, pkgbase: pkg.name);
                // invalidate all dashboard providers
                ref.invalidate(listActivitiesProvider);
                ref.invalidate(listPackagesProvider);
                ref.invalidate(listBuildsProvider);
                ref.invalidate(listStatsProvider);
                ref.invalidate(getGraphDataProvider);
              },
              () {},
            );
          },
          child: const Text(
            "Force Rebuild",
            style: TextStyle(color: Colors.yellowAccent),
          ),
        ),
        const SizedBox(width: 10),
        ElevatedButton(
          onPressed: () async {
            await showConfirmationDialog(
              context,
              "Delete Package",
              "Are you sure to delete this Package?",
              () async {
                final succ = await API.deletePackage(pkg.name);
                if (succ) {
                  // invalidate all dashboard providers
                  ref.invalidate(listActivitiesProvider);
                  ref.invalidate(listPackagesProvider);
                  ref.invalidate(listBuildsProvider);
                  ref.invalidate(listStatsProvider);
                  ref.invalidate(getGraphDataProvider);

                  if (mounted) {
                    context.pop();
                  }
                }
              },
              () {},
            );
          },
          child: const Text(
            "Delete",
            style: TextStyle(color: Colors.redAccent),
          ),
        ),
        const SizedBox(width: 10),
        ElevatedButton(
          onPressed: () async {
            await showPackageSourcePatchPopup(context, pkg.name);
            // A saved/reverted edit can change dependencies/version without
            // bumping the upstream version, so refresh everything relevant.
            ref.invalidate(getPackageProvider(pkg.name));
            ref.invalidate(listPackagesProvider);
            ref.invalidate(getGraphDataProvider);
          },
          child: Text(
            pkg.has_patch ? "Patch (active)" : "Patch",
            style: TextStyle(
              color: pkg.has_patch ? Colors.greenAccent : Colors.white,
            ),
          ),
        ),
        const SizedBox(width: 10),
        ElevatedButton(
          onPressed: () {
            context.push("/package/${pkg.id}/settings");
          },
          child: const Text(
            "Settings",
            style: TextStyle(color: Colors.blueAccent),
          ),
        ),
      ],
    );
  }

  Widget _buildSideBar(ExtendedPackage pkg) {
    return SizedBox(
      width: 300,
      child: Container(
        color: secondaryColor,
        padding: const EdgeInsets.all(defaultPadding),
        margin: const EdgeInsets.all(10),
        child: SingleChildScrollView(
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              const SizedBox(height: 5),
              Text(
                "Details for ${pkg.name}:",
                style: const TextStyle(fontSize: 18),
                textAlign: TextAlign.start,
              ),
              _sideCard(
                title: "Latest Upstream version",
                subtitle: pkg.upstream_version ?? '—',
              ),
              // The artifacts in the repository, once there are any. These
              // supersede the declared split-package names: they say which of
              // those actually built, for which platform, and how big each is.
              if (pkg.files != null && pkg.files!.isNotEmpty) ...[
                const SizedBox(height: 5),
                const Divider(),
                const SizedBox(height: 5),
                const Text(
                  "Files:",
                  style: TextStyle(fontSize: 18),
                  textAlign: TextAlign.start,
                ),
                const SizedBox(height: 12),
                ...pkg.files!.map(_fileRow),
                if (_totalSize(pkg.files!) != null) ...[
                  const SizedBox(height: 8),
                  Text(
                    "Total: ${_totalSize(pkg.files!)!.readableFileSize()}",
                    style: const TextStyle(fontWeight: FontWeight.bold),
                  ),
                ],
                const SizedBox(height: 5),
                const Divider(),
              ] else if (pkg.split_packages != null &&
                  pkg.split_packages!.length > 1) ...[
                const SizedBox(height: 5),
                const Divider(),
                const SizedBox(height: 5),
                const Text(
                  "Split packages:",
                  style: TextStyle(fontSize: 18),
                  textAlign: TextAlign.start,
                ),
                const SizedBox(height: 12),
                ...pkg.split_packages!.map(
                  (sp) => Padding(
                    padding: const EdgeInsets.only(bottom: 4),
                    child: Text(sp),
                  ),
                ),
                const SizedBox(height: 5),
                const Divider(),
              ],
              ...pkg.package_source.when(
                aur: (aur) {
                  final lastUpdated = DateTime.fromMillisecondsSinceEpoch(
                    (pkg.last_modified ?? 0) * 1000,
                  );
                  final firstSubmitted = DateTime.fromMillisecondsSinceEpoch(
                    (pkg.first_submitted ?? 0) * 1000,
                  );

                  return [
                    _sideCard(
                      title: "Last Updated",
                      subtitle:
                          "${lastUpdated.year}-${lastUpdated.month.toString().padLeft(2, '0')}-${lastUpdated.day.toString().padLeft(2, '0')}",
                    ),
                    _sideCard(
                      title: "First submitted",
                      subtitle:
                          "${firstSubmitted.year}-${firstSubmitted.month.toString().padLeft(2, '0')}-${firstSubmitted.day.toString().padLeft(2, '0')}",
                    ),
                    _sideCard(title: "Licenses", subtitle: pkg.licenses ?? "-"),
                    _sideCard(
                      title: "Maintainer",
                      subtitle: pkg.maintainer ?? "-",
                    ),
                    _sideCard(
                      title: "Flagged outdated",
                      subtitle: aur.aur_flagged_outdated ? "yes" : "no",
                    ),
                  ];
                },
                aurNotFound: (_) => [],
                git: (git) => [
                  _sideCard(title: "Git Repository", subtitle: git.url),
                  _sideCard(title: "Git Ref", subtitle: git.ref),
                  _sideCard(title: "Subfolder", subtitle: git.subfolder),
                ],
                upload: (upload) => {
                  // todo upload type
                },
              ),
              const Divider(),
              const SizedBox(height: 5),
              const Text(
                "Dependencies:",
                style: TextStyle(fontSize: 18),
                textAlign: TextAlign.start,
              ),
              const SizedBox(height: 12),
              _buildPackageLinks(pkg.dependencies),
              const SizedBox(height: 15),
              const Divider(),
              const SizedBox(height: 5),
              const Text(
                "Dependents:",
                style: TextStyle(fontSize: 18),
                textAlign: TextAlign.start,
              ),
              const SizedBox(height: 12),
              _buildPackageLinks(pkg.dependents),
              const SizedBox(height: 15),
              const Divider(),
              const SizedBox(height: 5),
              const Text(
                "Selected build platforms:",
                style: TextStyle(fontSize: 18),
                textAlign: TextAlign.start,
              ),
              const SizedBox(height: 20),
              Tags(
                itemBuilder: (idx) => ItemTags(
                  index: idx,
                  title: pkg.selected_platforms[idx],
                  active: true,
                  activeColor: Colors.green,
                  pressEnabled: false,
                ),
                itemCount: pkg.selected_platforms.length,
              ),
              const SizedBox(height: 15),
              const Divider(),
              const SizedBox(height: 5),
              const Text(
                "Build flags:",
                style: TextStyle(fontSize: 18),
                textAlign: TextAlign.start,
              ),
              const SizedBox(height: 20),
              Tags(
                itemBuilder: (idx) => ItemTags(
                  index: idx,
                  title: pkg.selected_build_flags[idx],
                  active: true,
                  activeColor: Colors.white38,
                  pressEnabled: false,
                ),
                itemCount: pkg.selected_build_flags.length,
              ),
            ],
          ),
        ),
      ),
    );
  }

  Widget _buildPackageLinks(List<PackageDependency> packages) {
    if (packages.isEmpty) {
      return const Text("none");
    }

    final sortedPackages = [...packages]
      ..sort((a, b) => a.name.toLowerCase().compareTo(b.name.toLowerCase()));

    return Wrap(
      spacing: 8,
      runSpacing: 8,
      children: sortedPackages
          .map((package) {
            final suffix = package.version_constraint.isEmpty
                ? ''
                : ' ${package.version_constraint}';
            return ActionChip(
              label: Text('${package.name}$suffix'),
              onPressed: () {
                context.push("/package/${package.id}");
              },
            );
          })
          .toList(growable: false),
    );
  }

  /// One artifact: its filename, the platform it was built for, and its size.
  ///
  /// A null size renders as a dash, not as "0 B" — it means the size is not
  /// recorded for that row, not that the package file is empty.
  Widget _fileRow(PackageFile file) {
    return Padding(
      padding: const EdgeInsets.only(bottom: 6),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(file.filename, style: const TextStyle(fontSize: 13)),
          Text(
            "${file.platform} · ${file.size?.readableFileSize() ?? '—'}",
            style: const TextStyle(fontSize: 12, color: Colors.grey),
          ),
        ],
      ),
    );
  }

  /// Combined size of every artifact, or null if any one of them is unknown.
  ///
  /// All-or-nothing on purpose: adding up only the known sizes would show a
  /// total smaller than the rows above it, which reads as a bug rather than as
  /// missing data.
  int? _totalSize(List<PackageFile> files) {
    var total = 0;
    for (final file in files) {
      final size = file.size;
      if (size == null) return null;
      total += size;
    }
    return total;
  }

  Widget _sideCard({required String title, required String subtitle}) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const SizedBox(height: 5),
        Text(
          title,
          style: TextStyle(fontSize: 13, fontWeight: FontWeight.bold),
        ),
        const SizedBox(height: 3),
        Text(subtitle),
        const SizedBox(height: 10),
      ],
    );
  }

  Widget _buildMainBody(ExtendedPackage pkg) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        ...pkg.package_source.when(
          aur: (aur) {
            if (pkg.description != null) {
              return [
                const SizedBox(height: 25),
                Padding(
                  padding: const EdgeInsets.all(5.0),
                  child: Text(pkg.description!),
                ),
                const SizedBox(height: 25),
              ];
            } else {
              return [const SizedBox(height: 25)];
            }
          },
          aurNotFound: (_) => [
            const SizedBox(height: 25),
            Container(
              padding: const EdgeInsets.all(12),
              decoration: BoxDecoration(
                color: Colors.orange.withValues(alpha: 0.15),
                border: Border.all(color: Colors.orange),
                borderRadius: BorderRadius.circular(8),
              ),
              child: const Row(
                children: [
                  Icon(Icons.warning_amber_rounded, color: Colors.orange),
                  SizedBox(width: 10),
                  Expanded(
                    child: Text(
                      'This package is no longer available in the AUR. '
                      'It may have been moved to the official repositories or deleted. '
                      'You can delete it using the button above.',
                    ),
                  ),
                ],
              ),
            ),
            const SizedBox(height: 25),
          ],
          git: (git) {
            // todo description from git
            return [const SizedBox(height: 25)];
          },
          upload: (upload) {
            return [];
          },
        ),
        Expanded(
          child: Container(
            padding: const EdgeInsets.all(defaultPadding),
            decoration: const BoxDecoration(
              color: secondaryColor,
              borderRadius: BorderRadius.all(Radius.circular(10)),
            ),
            child: Column(
              crossAxisAlignment: CrossAxisAlignment.stretch,
              mainAxisAlignment: MainAxisAlignment.center,
              children: [
                Text(
                  "Builds of ${pkg.name}",
                  style: Theme.of(context).textTheme.titleMedium,
                ),
                Expanded(
                  child: SingleChildScrollView(
                    child: APIBuilder(
                      interval: const Duration(seconds: 30),
                      onData: (List<Build> data) {
                        return BuildsTable(data: data);
                      },
                      onLoad: () => const Text("no data"),
                      provider: listBuildsProvider(pkgbase: pkg.name),
                    ),
                  ),
                ),
              ],
            ),
          ),
        ),
      ],
    );
  }
}
