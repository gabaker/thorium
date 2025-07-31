// Populate the sidebar
//
// This is a script, and not included directly in the page, to control the total size of the book.
// The TOC contains an entry for each page, so if each page includes a copy of the TOC,
// the total size of the page becomes O(n**2).
class MDBookSidebarScrollbox extends HTMLElement {
    constructor() {
        super();
    }
    connectedCallback() {
        this.innerHTML = '<ol class="chapter"><li class="chapter-item expanded "><a href="intro.html"><strong aria-hidden="true">1.</strong> Intro</a></li><li class="chapter-item expanded "><a href="getting_started/getting_started.html"><strong aria-hidden="true">2.</strong> Getting Started</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="getting_started/registration.html"><strong aria-hidden="true">2.1.</strong> Registration</a></li><li class="chapter-item "><a href="getting_started/login.html"><strong aria-hidden="true">2.2.</strong> Login</a></li><li class="chapter-item "><a href="getting_started/roles_permissions.html"><strong aria-hidden="true">2.3.</strong> Roles and Permissions</a></li><li class="chapter-item "><a href="getting_started/adding_editing_groups.html"><strong aria-hidden="true">2.4.</strong> Adding/Editing Groups</a></li><li class="chapter-item "><a href="getting_started/thorctl.html"><strong aria-hidden="true">2.5.</strong> Command Line Tool</a></li></ol></li><li class="chapter-item expanded "><a href="users/users.html"><strong aria-hidden="true">3.</strong> Users</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="users/uploading_files.html"><strong aria-hidden="true">3.1.</strong> Uploading Files</a></li><li class="chapter-item "><a href="users/viewing_files.html"><strong aria-hidden="true">3.2.</strong> Viewing Files</a></li><li class="chapter-item "><a href="users/tagging_files.html"><strong aria-hidden="true">3.3.</strong> Tagging Files</a></li><li class="chapter-item "><a href="users/spawning_reactions.html"><strong aria-hidden="true">3.4.</strong> Spawning Reactions</a></li><li class="chapter-item "><a href="users/viewing_results.html"><strong aria-hidden="true">3.5.</strong> Viewing Tool Results</a></li><li class="chapter-item "><a href="users/searching_results.html"><strong aria-hidden="true">3.6.</strong> Searching Tool Results</a></li><li class="chapter-item "><a href="users/downloading.html"><strong aria-hidden="true">3.7.</strong> Downloading Files</a></li><li class="chapter-item "><a href="users/commenting.html"><strong aria-hidden="true">3.8.</strong> Commenting on Files</a></li><li class="chapter-item "><a href="users/revoking_token.html"><strong aria-hidden="true">3.9.</strong> Revoking Your Token</a></li></ol></li><li class="chapter-item expanded "><a href="developers/developers.html"><strong aria-hidden="true">4.</strong> Tool Developers</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="developers/images.html"><strong aria-hidden="true">4.1.</strong> Working With Tools</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="developers/add_images.html"><strong aria-hidden="true">4.1.1.</strong> Adding Images</a></li><li class="chapter-item "><a href="developers/configuring_images.html"><strong aria-hidden="true">4.1.2.</strong> Configuring Images</a></li><li class="chapter-item "><a href="developers/children.html"><strong aria-hidden="true">4.1.3.</strong> More on Children (Samples)</a></li></ol></li><li class="chapter-item "><a href="developers/build_pipelines.html"><strong aria-hidden="true">4.2.</strong> Building Pipelines</a></li><li class="chapter-item "><a href="developers/reaction_status.html"><strong aria-hidden="true">4.3.</strong> Reaction Status</a></li><li class="chapter-item "><a href="developers/viewing_reaction_logs.html"><strong aria-hidden="true">4.4.</strong> Viewing Reaction Logs</a></li><li class="chapter-item "><a href="developers/generators.html"><strong aria-hidden="true">4.5.</strong> Generators</a></li><li class="chapter-item "><a href="developers/bans.html"><strong aria-hidden="true">4.6.</strong> Bans</a></li><li class="chapter-item "><a href="developers/notifications.html"><strong aria-hidden="true">4.7.</strong> Notifications</a></li></ol></li><li class="chapter-item expanded "><a href="admins/admins.html"><strong aria-hidden="true">5.</strong> Admins</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="architecture/architecture.html"><strong aria-hidden="true">5.1.</strong> Architecture</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="architecture/api.html"><strong aria-hidden="true">5.1.1.</strong> API</a></li><li class="chapter-item "><a href="architecture/scaler.html"><strong aria-hidden="true">5.1.2.</strong> Scaler</a></li><li class="chapter-item "><a href="architecture/agent.html"><strong aria-hidden="true">5.1.3.</strong> Agent</a></li><li class="chapter-item "><a href="architecture/reactor.html"><strong aria-hidden="true">5.1.4.</strong> Reactor</a></li><li class="chapter-item "><a href="architecture/tracing.html"><strong aria-hidden="true">5.1.5.</strong> Tracing/Logging</a></li><li class="chapter-item "><a href="architecture/event-handler.html"><strong aria-hidden="true">5.1.6.</strong> Event Handler</a></li></ol></li><li class="chapter-item "><a href="admins/deploy/deploy.html"><strong aria-hidden="true">5.2.</strong> Deploy</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="admins/deploy/install-traefik.html"><strong aria-hidden="true">5.2.1.</strong> Traefik</a></li><li class="chapter-item "><a href="admins/deploy/install-rook.html"><strong aria-hidden="true">5.2.2.</strong> Rook</a></li><li class="chapter-item "><a href="admins/deploy/install-redis.html"><strong aria-hidden="true">5.2.3.</strong> Redis</a></li><li class="chapter-item "><a href="admins/deploy/install-scylla.html"><strong aria-hidden="true">5.2.4.</strong> Scylla</a></li><li class="chapter-item "><a href="admins/deploy/install-elastic.html"><strong aria-hidden="true">5.2.5.</strong> ECK</a></li><li class="chapter-item "><a href="admins/deploy/install-tracing.html"><strong aria-hidden="true">5.2.6.</strong> Tracing</a></li><li class="chapter-item "><a href="admins/deploy/deploy-thorium.html"><strong aria-hidden="true">5.2.7.</strong> Thorium</a></li></ol></li><li class="chapter-item "><a href="admins/operations.html"><strong aria-hidden="true">5.3.</strong> Operations</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="admins/troubleshoot_user_issues.html"><strong aria-hidden="true">5.3.1.</strong> Troubleshoot WebUI Issues</a></li><li class="chapter-item "><a href="admins/delete_group.html"><strong aria-hidden="true">5.3.2.</strong> Delete A Group</a></li><li class="chapter-item "><a href="admins/delete_user.html"><strong aria-hidden="true">5.3.3.</strong> Delete A User</a></li><li class="chapter-item "><a href="admins/bans_admins.html"><strong aria-hidden="true">5.3.4.</strong> Ban Things in Thorium</a></li><li class="chapter-item "><a href="admins/notifications_admins.html"><strong aria-hidden="true">5.3.5.</strong> Create Notifications</a></li></ol></li><li class="chapter-item "><a href="admins/thoradm/thoradm.html"><strong aria-hidden="true">5.4.</strong> Admin Command Line Tool</a></li><li class="chapter-item "><a href="admins/common_issues.html"><strong aria-hidden="true">5.5.</strong> Common Issues</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="admins/common_issues/jobs_stuck_at_created.html"><strong aria-hidden="true">5.5.1.</strong> Jobs Stuck At Created</a></li></ol></li><li class="chapter-item "><a href="admins/network_policies/network_policies.html"><strong aria-hidden="true">5.6.</strong> Network Policies</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="admins/network_policies/configure_network_policies.html"><strong aria-hidden="true">5.6.1.</strong> Configuring Network Policies</a></li></ol></li></ol></li><li class="chapter-item expanded "><a href="help/help.html"><strong aria-hidden="true">6.</strong> Getting Help</a><a class="toggle"><div>❱</div></a></li><li><ol class="section"><li class="chapter-item "><a href="help/faq.html"><strong aria-hidden="true">6.1.</strong> FAQ</a></li><li class="chapter-item "><a href="help/contact_us.html"><strong aria-hidden="true">6.2.</strong> Contact Us</a></li></ol></li></ol>';
        // Set the current, active page, and reveal it if it's hidden
        let current_page = document.location.href.toString().split("#")[0];
        if (current_page.endsWith("/")) {
            current_page += "index.html";
        }
        var links = Array.prototype.slice.call(this.querySelectorAll("a"));
        var l = links.length;
        for (var i = 0; i < l; ++i) {
            var link = links[i];
            var href = link.getAttribute("href");
            if (href && !href.startsWith("#") && !/^(?:[a-z+]+:)?\/\//.test(href)) {
                link.href = path_to_root + href;
            }
            // The "index" page is supposed to alias the first chapter in the book.
            if (link.href === current_page || (i === 0 && path_to_root === "" && current_page.endsWith("/index.html"))) {
                link.classList.add("active");
                var parent = link.parentElement;
                if (parent && parent.classList.contains("chapter-item")) {
                    parent.classList.add("expanded");
                }
                while (parent) {
                    if (parent.tagName === "LI" && parent.previousElementSibling) {
                        if (parent.previousElementSibling.classList.contains("chapter-item")) {
                            parent.previousElementSibling.classList.add("expanded");
                        }
                    }
                    parent = parent.parentElement;
                }
            }
        }
        // Track and set sidebar scroll position
        this.addEventListener('click', function(e) {
            if (e.target.tagName === 'A') {
                sessionStorage.setItem('sidebar-scroll', this.scrollTop);
            }
        }, { passive: true });
        var sidebarScrollTop = sessionStorage.getItem('sidebar-scroll');
        sessionStorage.removeItem('sidebar-scroll');
        if (sidebarScrollTop) {
            // preserve sidebar scroll position when navigating via links within sidebar
            this.scrollTop = sidebarScrollTop;
        } else {
            // scroll sidebar to current active section when navigating via "next/previous chapter" buttons
            var activeSection = document.querySelector('#sidebar .active');
            if (activeSection) {
                activeSection.scrollIntoView({ block: 'center' });
            }
        }
        // Toggle buttons
        var sidebarAnchorToggles = document.querySelectorAll('#sidebar a.toggle');
        function toggleSection(ev) {
            ev.currentTarget.parentElement.classList.toggle('expanded');
        }
        Array.from(sidebarAnchorToggles).forEach(function (el) {
            el.addEventListener('click', toggleSection);
        });
    }
}
window.customElements.define("mdbook-sidebar-scrollbox", MDBookSidebarScrollbox);
