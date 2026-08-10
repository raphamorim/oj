// Linked only from a below-the-fold anchor on the home page, so it isn't
// viewport-prefetched on load — used to distinguish hover prefetch from
// viewport prefetch.
export default function Deep() {
  return (
    <main data-page="deep">
      <h1>deep</h1>
      <a href="/">home</a>
    </main>
  );
}
