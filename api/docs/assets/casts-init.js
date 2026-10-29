// Initialize an asciinema player for every <div class="asciinema-clip" data-cast="..."> on
// the page. mdbook injects additional-js at the end of the body, after page content, so this
// must run on window load rather than inline in the page.
window.addEventListener('load', function () {
  document.querySelectorAll('div.asciinema-clip[data-cast]').forEach(function (el) {
    // the cast file carries its own terminal geometry; fit it to the container width
    AsciinemaPlayer.create(el.dataset.cast, el, {
      fit: 'width',
      idleTimeLimit: 2,
      preload: true,
      poster: 'npt:0:1',
    });
  });
});
