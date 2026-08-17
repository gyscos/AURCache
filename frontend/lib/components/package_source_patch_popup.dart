import 'package:flutter/material.dart';

import '../api/API.dart';
import '../api/source_edit.dart';

/// Dialog for browsing/editing the source files of an already-added
/// package. Each file always has pristine content available to fall back
/// to/revert to, and may additionally have patched content (if it's part of
/// the package's stored patch and that patch still applies) or a patch
/// error (if it's part of the patch but no longer applies cleanly, e.g.
/// after an upstream update).
class PackageSourcePatchPopup extends StatefulWidget {
  const PackageSourcePatchPopup({super.key, required this.packageId});

  final int packageId;

  @override
  State<PackageSourcePatchPopup> createState() =>
      _PackageSourcePatchPopupState();
}

class _PackageSourcePatchPopupState extends State<PackageSourcePatchPopup> {
  bool _loadingFiles = true;
  bool _loadingFile = false;
  bool _saving = false;

  List<String> _files = [];
  String? _selectedFile;
  final _controller = TextEditingController();

  String? _originalContent;
  String? _patchError;
  // Whether the currently-loaded file has a patch applied to it (rather
  // than falling back to original content because there is no patch for
  // this file, or because the stored patch no longer applies).
  bool _isPatched = false;

  String? _error;

  @override
  void initState() {
    super.initState();
    _loadFiles();
  }

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  Future<void> _loadFiles() async {
    setState(() {
      _loadingFiles = true;
      _error = null;
    });
    try {
      final files = await API.getSourceFiles(widget.packageId);
      setState(() => _files = files);
      final preferred = files.firstWhere(
        (f) => f == 'PKGBUILD',
        orElse: () => files.isNotEmpty ? files.first : '',
      );
      if (preferred.isNotEmpty) {
        await _selectFile(preferred);
      }
    } catch (e) {
      setState(() => _error = 'Failed to list source files: $e');
    } finally {
      if (mounted) setState(() => _loadingFiles = false);
    }
  }

  Future<void> _selectFile(String path) async {
    setState(() {
      _loadingFile = true;
      _selectedFile = path;
      _error = null;
    });
    try {
      final content = await API.getSourceFile(
        id: widget.packageId,
        path: path,
      );
      _originalContent = content.originalContent;
      _patchError = content.patchError;
      _isPatched = content.patchedContent != null;
      _controller.text = content.patchedContent ?? content.originalContent;
    } catch (e) {
      setState(() => _error = 'Failed to read $path: $e');
    } finally {
      if (mounted) setState(() => _loadingFile = false);
    }
  }

  Future<void> _save() async {
    final path = _selectedFile;
    if (path == null) return;
    setState(() {
      _saving = true;
      _error = null;
    });
    try {
      await API.updateSourceFile(
        id: widget.packageId,
        path: path,
        content: _controller.text,
      );
      setState(() {
        _isPatched = _controller.text != _originalContent;
        _patchError = null;
      });
    } catch (e) {
      setState(() => _error = 'Failed to save $path: $e');
    } finally {
      if (mounted) setState(() => _saving = false);
    }
  }

  void _revert() {
    if (_originalContent == null) return;
    setState(() {
      _controller.text = _originalContent!;
    });
  }

  @override
  Widget build(BuildContext context) {
    final dirty = _controller.text != (_originalContent ?? '');

    return AlertDialog(
      title: const Text('Patch package sources'),
      content: SizedBox(
        width: 700,
        height: 450,
        child: _loadingFiles
            ? const Center(child: CircularProgressIndicator())
            : Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  if (_error != null)
                    Padding(
                      padding: const EdgeInsets.only(bottom: 8),
                      child: Text(
                        _error!,
                        style: const TextStyle(color: Colors.red),
                      ),
                    ),
                  if (_patchError != null)
                    Container(
                      width: double.infinity,
                      color: Colors.orange.withValues(alpha: 0.15),
                      padding: const EdgeInsets.all(6),
                      margin: const EdgeInsets.only(bottom: 8),
                      child: Text(
                        'The stored patch for this file no longer applies '
                        'cleanly (upstream likely changed): $_patchError\n'
                        'Showing the original content instead - edit and '
                        'save to replace the patch, or revert to drop it.',
                        style: TextStyle(
                          fontSize: 11,
                          color: Colors.orange[900],
                        ),
                      ),
                    ),
                  Expanded(
                    child: Row(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        SizedBox(
                          width: 160,
                          child: ListView.builder(
                            itemCount: _files.length,
                            itemBuilder: (context, index) {
                              final f = _files[index];
                              return ListTile(
                                dense: true,
                                selected: f == _selectedFile,
                                title: Text(
                                  f,
                                  style: const TextStyle(fontSize: 12),
                                ),
                                onTap: () => _selectFile(f),
                              );
                            },
                          ),
                        ),
                        const VerticalDivider(width: 1),
                        Expanded(
                          child: Column(
                            crossAxisAlignment: CrossAxisAlignment.stretch,
                            children: [
                              if (_selectedFile != null)
                                Padding(
                                  padding: const EdgeInsets.only(bottom: 4),
                                  child: Text(
                                    _isPatched
                                        ? '${_selectedFile!} (patched)'
                                        : _selectedFile!,
                                    style: const TextStyle(
                                      fontWeight: FontWeight.bold,
                                    ),
                                  ),
                                ),
                              Expanded(
                                child: _loadingFile
                                    ? const Center(
                                        child: CircularProgressIndicator(),
                                      )
                                    : TextField(
                                        controller: _controller,
                                        onChanged: (_) => setState(() {}),
                                        maxLines: null,
                                        expands: true,
                                        textAlignVertical:
                                            TextAlignVertical.top,
                                        style: const TextStyle(
                                          fontFamily: 'monospace',
                                          fontSize: 12,
                                        ),
                                        decoration: const InputDecoration(
                                          border: OutlineInputBorder(),
                                          contentPadding: EdgeInsets.all(8),
                                        ),
                                      ),
                              ),
                            ],
                          ),
                        ),
                      ],
                    ),
                  ),
                ],
              ),
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Close'),
        ),
        if (_isPatched)
          TextButton.icon(
            onPressed: _saving ? null : _revert,
            icon: const Icon(Icons.restore, size: 16),
            label: const Text('Revert'),
          ),
        FilledButton.icon(
          onPressed: (dirty && !_saving && _selectedFile != null)
              ? _save
              : null,
          icon: _saving
              ? const SizedBox(
                  width: 14,
                  height: 14,
                  child: CircularProgressIndicator(strokeWidth: 2),
                )
              : const Icon(Icons.check, size: 16),
          label: const Text('Save'),
        ),
      ],
    );
  }
}

Future<void> showPackageSourcePatchPopup(
  BuildContext context,
  int packageId,
) {
  return showDialog(
    context: context,
    builder: (context) => PackageSourcePatchPopup(packageId: packageId),
  );
}
