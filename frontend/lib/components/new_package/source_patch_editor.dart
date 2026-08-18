import 'package:flutter/material.dart';

import '../../api/API.dart';
import '../../api/source_preview.dart';

/// Inline editor, embeddable in the "Add Package" wizard, that lets the user
/// browse the files of a not-yet-added source (AUR/Git) and edit one of them
/// (typically PKGBUILD) before the package is created. Edits are kept
/// entirely client-side as full file contents (path -> new content) and
/// surfaced via [onPatchedFilesChanged] so the caller can send them along
/// with `POST /package` as `patched_files` - the backend diffs each entry
/// against the source's pristine content itself when the package is added.
///
/// This is especially useful for AUR packages whose upstream PKGBUILD/
/// .SRCINFO fails to parse: the user can fix it up here first instead of
/// only being able to add it once already-valid.
class SourcePatchEditor extends StatefulWidget {
  const SourcePatchEditor({
    super.key,
    required this.source,
    required this.onPatchedFilesChanged,
  });

  /// Returns the current `source` request body (AUR/Git spec), or null if
  /// not enough information has been entered yet (e.g. no package selected).
  final Map<String, dynamic>? Function() source;

  final void Function(Map<String, String>? patchedFiles) onPatchedFilesChanged;

  @override
  State<SourcePatchEditor> createState() => _SourcePatchEditorState();
}

class _SourcePatchEditorState extends State<SourcePatchEditor> {
  bool _expanded = false;
  bool _loadingFiles = false;
  bool _loadingFile = false;

  List<String> _files = [];
  String? _selectedFile;
  final _controller = TextEditingController();

  // Pristine content of each file already fetched, keyed by path - used as
  // the baseline to detect whether an edit actually changed anything.
  final Map<String, String> _originalContent = {};
  // Edited content the user has saved for a file, keyed by path. Only
  // contains entries that differ from `_originalContent`.
  final Map<String, String> _editedContent = {};

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

    // Prefer whatever the user has already edited this session, so
    // switching files and back doesn't lose unsaved-but-applied edits.
    if (_editedContent.containsKey(path)) {
      setState(() {
        _selectedFile = path;
        _controller.text = _editedContent[path]!;
        _error = null;
      });
      return;
    }

    setState(() {
      _loadingFile = true;
      _selectedFile = path;
      _error = null;
    });
    try {
      final content =
          _originalContent[path] ??
          await API.previewSourceFile(source: source, path: path);
      _originalContent[path] = content;
      _controller.text = content;
    } catch (e) {
      setState(() => _error = 'Failed to read $path: $e');
    } finally {
      if (mounted) setState(() => _loadingFile = false);
    }
  }

  void _applyEdit() {
    final path = _selectedFile;
    if (path == null) return;
    final original = _originalContent[path];
    final newContent = _controller.text;
    setState(() {
      if (original == newContent) {
        _editedContent.remove(path);
      } else {
        _editedContent[path] = newContent;
      }
    });
    widget.onPatchedFilesChanged(
      _editedContent.isEmpty ? null : Map.of(_editedContent),
    );
  }

  void _revert() {
    final path = _selectedFile;
    if (path == null) return;
    final original = _originalContent[path];
    if (original == null) return;
    setState(() {
      _controller.text = original;
      _editedContent.remove(path);
    });
    widget.onPatchedFilesChanged(
      _editedContent.isEmpty ? null : Map.of(_editedContent),
    );
  }

  @override
  Widget build(BuildContext context) {
    final dirty =
        _selectedFile != null &&
        _controller.text != (_originalContent[_selectedFile] ?? '');
    final hasEdits = _editedContent.isNotEmpty;

    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        TextButton.icon(
          onPressed: _toggleExpanded,
          icon: Icon(_expanded ? Icons.expand_less : Icons.edit_note),
          label: Text(
            _expanded
                ? 'Hide source editor'
                : (hasEdits
                      ? 'Edit source files (${_editedContent.length} edited)'
                      : 'Edit source files (e.g. fix PKGBUILD)'),
          ),
        ),
        if (_error != null)
          Padding(
            padding: const EdgeInsets.only(bottom: 8),
            child: Text(_error!, style: const TextStyle(color: Colors.red)),
          ),
        if (_expanded) _buildEditor(context, dirty),
      ],
    );
  }

  Widget _buildEditor(BuildContext context, bool dirty) {
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
                  title: Text(
                    _editedContent.containsKey(f) ? '$f *' : f,
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
                Expanded(
                  child: _loadingFile
                      ? const Center(child: CircularProgressIndicator())
                      : Padding(
                          padding: const EdgeInsets.all(8),
                          child: Container(
                            decoration: BoxDecoration(
                              border: Border.all(
                                color: Theme.of(context).colorScheme.outline,
                              ),
                            ),
                            child: SingleChildScrollView(
                              padding: const EdgeInsets.all(8),
                              child: TextField(
                                controller: _controller,
                                onChanged: (_) => setState(() {}),
                                maxLines: null,
                                textAlignVertical: TextAlignVertical.top,
                                style: const TextStyle(
                                  fontFamily: 'monospace',
                                  fontSize: 12,
                                ),
                                decoration: const InputDecoration(
                                  border: InputBorder.none,
                                  isDense: true,
                                ),
                              ),
                            ),
                          ),
                        ),
                ),
                Padding(
                  padding: const EdgeInsets.all(8),
                  child: Row(
                    mainAxisAlignment: MainAxisAlignment.end,
                    children: [
                      if (_selectedFile != null &&
                          _editedContent.containsKey(_selectedFile))
                        TextButton.icon(
                          onPressed: _revert,
                          icon: const Icon(Icons.restore, size: 16),
                          label: const Text('Revert'),
                        ),
                      const SizedBox(width: 8),
                      FilledButton.icon(
                        onPressed: (dirty && _selectedFile != null)
                            ? _applyEdit
                            : null,
                        icon: const Icon(Icons.check, size: 16),
                        label: const Text('Apply edit'),
                      ),
                    ],
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
