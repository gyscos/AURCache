use aurcache_common::api::activity::ActivitySubject;

pub trait ActivitySerializer {
    fn format(&self) -> String;

    /// What this entry is about, when the UI has somewhere to send a reader.
    ///
    /// Defaults to nothing, so an entry only claims a subject when linking to
    /// it actually leads somewhere: a deletion names a package that is gone,
    /// and a link to it would be a page that 404s.
    fn subject(&self) -> Option<ActivitySubject> {
        None
    }
}
