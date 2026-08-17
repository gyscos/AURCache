import 'package:flutter/material.dart';

import '../../api/API.dart';
import '../../api/source_preview.dart';

/// Inline editor, embeddable in the "Add Package" wizard, that lets the user
/// browse the files of a not-yet-added source (AUR/Git) and edit one of them
/// (typically PKGBUILD) before the package is created. Edits are merged into
/// a multi-file patch (same format used post-add) which is surfaced via
/// [onPatchChanged] so the caller can send it along with `POST /package`.
///
/// This is especially useful for AUR packages whose upstream PKGBUILD/
/// .SRCINFO fails to parse: the user can fix it up here first instead of
/// only being able to add it once already-valid.
class SourcePatchEditor extends StatefulWidget {
  const SourcePatchEditor({
    super.key,
    required this.source,
    required this.onPatchChanged,
  });

  /// Returns the current `source` request body (AUR/Git spec), or null if
  /// not enough information has been entered yet (e.g. no package selected).
  final Map<String, dynamic>? Function() source;

  final void Function(String? patch) onPatchChanged;

  @override
  State<SourcePatchEditor> createState() => _SourcePatchEditorState();
}

class _SourcePatchEditorState extends State<SourcePatchEditor> {
  bool _expanded = false;
  bool _loadingFiles = false;
  bool _loadingFile = false;
  bool _saving = false;

  List<String> _files = [];
  String? _selectedFile;
  final _controller = TextEditingController();

  String? _patch;
  bool _dirty = false;
  bool? _parses;
  String? _parseError;
  String? _error;

  @override
  void dispose() {
    _controller.dispose();
    super.dispose();
  }

  Future<void> _toggleExpanded() async {
    final source = widget.source();
    if (source == null) {
      setState(() {
        _error = 'Fill in the source details above first.';
      });
      return;
    }
    setState(() {
      _expanded = !_expanded;
      _error = null;
    });
    if (_expanded && _files.isEmpty) {
      await _loadFiles();
    }
  }

  Future<void> _loadFiles() async {
    final source = widget.source();
    if (source == null) return;
    setState(() {
      _loadingFiles = true;
      _error = null;
    });
    try {
      final files = await API.previewSourceFiles(source: source);
      setState(() {
        _files = files;
      });
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
    final source = widget.source();
    if (source == null) return;
    setState(() {
      _loadingFile = true;
      _selectedFile = path;
      _error = null;
    });
    try {
      final content = await API.previewSourceFile(
        source: source,
        patch: _patch,
        path: path,
      );
      _controller.text = content;
      setState(() => _dirty = false);
    } catch (e) {
      setState(() => _error = 'Failed to read $path: $e');
    } finally {
      if (mounted) setState(() => _loadingFile = false);
    }
  }

  Future<void> _saveEdit() async {
    final source = widget.source();
    if (source == null || _selectedFile == null) return;
    setState(() {
      _saving = true;
      _error = null;
    });
    try {
      final result = await API.updatePreviewSourceFile(
        source: source,
        patch: _patch,
        path: _selectedFile!,
        content: _controller.text,
      );
      setState(() {
        _patch = result.patch;
        _parses = result.parses;
        _parseError = result.parseError;
        _dirty = false;
      });
      widget.onPatchChanged(_patch);
    } catch (e) {
      setState(() => _error = 'Failed to apply edit: $e');
    } finally {
      if (mounted) setState(() => _saving = false);
    }
  }

  @override
  Widget build(BuildContext context) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        TextButton.icon(
          onPressed: _toggleExpanded,
          icon: Icon(_expanded ? Icons.expand_less : Icons.edit_note),
          label: Text(
            _expanded
                ? 'Hide source editor'
                : (_patch != null
                      ? 'Edit source files (patch applied)'
                      : 'Edit source files (e.g. fix PKGBUILD)'),
          ),
        ),
        if (_error != null)
          Padding(
            padding: const EdgeInsets.only(bottom: 8),
            child: Text(_error!, style: const TextStyle(color: Colors.red)),
          ),
        if (_expanded) _buildEditor(context),
      ],
    );
  }

  Widget _buildEditor(BuildContext context) {
    if (_loadingFiles) {
      return const Padding(
        padding: EdgeInsets.all(16),
        child: Center(child: CircularProgressIndicator()),
      );
    }
    return SizedBox(
      height: 300,
      child: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 140,
            child: ListView.builder(
              itemCount: _files.length,
              itemBuilder: (context, index) {
                final f = _files[index];
                return ListTile(
                  dense: true,
                  selected: f == _selectedFile,
                  title: Text(f, style: const TextStyle(fontSize: 12)),
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
                if (_parses != null)
                  Container(
                    color: _parses!
                        ? Colors.green.withValues(alpha: 0.15)
                        : Colors.orange.withValues(alpha: 0.15),
                    padding: const EdgeInsets.all(6),
                    child: Text(
                      _parses!
                          ? 'Patched source parses correctly.'
                          : 'Patched source still fails to parse: ${_parseError ?? 'unknown error'}',
                      style: TextStyle(
                        fontSize: 11,
                        color: _parses! ? Colors.green[800] : Colors.orange[900],
                      ),
                    ),
                  ),
                Expanded(
                  child: _loadingFile
                      ? const Center(child: CircularProgressIndicator())
                      : Padding(
                          padding: const EdgeInsets.all(8),
                          child: TextField(
                            controller: _controller,
                            onChanged: (_) => setState(() => _dirty = true),
                            maxLines: null,
                            expands: true,
                            textAlignVertical: TextAlignVertical.top,
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
                ),
                Padding(
                  padding: const EdgeInsets.all(8),
                  child: Align(
                    alignment: Alignment.centerRight,
                    child: FilledButton.icon(
                      onPressed: (_dirty && !_saving && _selectedFile != null)
                          ? _saveEdit
                          : null,
                      icon: _saving
                          ? const SizedBox(
                              width: 14,
                              height: 14,
                              child: CircularProgressIndicator(strokeWidth: 2),
                            )
                          : const Icon(Icons.check, size: 16),
                      label: const Text('Apply edit to patch'),
                    ),
                  ),
                ),
              ],
            ),
          ),
        ],
      ),
    );
  }
}
