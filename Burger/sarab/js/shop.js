/* ============================================================
   SHOP LAYER - accounts, wishlist, menu tools, checkout, orders
   Front-end only: data lives in this browser's localStorage.
   ============================================================ */
(function () {
    'use strict';

    /* ---------- helpers ---------- */
    var store = {
        get: function (k, d) {
            try { var v = localStorage.getItem(k); return v ? JSON.parse(v) : d; } catch (e) { return d; }
        },
        set: function (k, v) {
            try { localStorage.setItem(k, JSON.stringify(v)); } catch (e) { /* storage blocked */ }
        },
        del: function (k) { try { localStorage.removeItem(k); } catch (e) {} }
    };
    function q(s, r) { return (r || document).querySelector(s); }
    function qa(s, r) { return Array.prototype.slice.call((r || document).querySelectorAll(s)); }
    function esc(s) {
        return String(s == null ? '' : s).replace(/[&<>"']/g, function (c) {
            return { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c];
        });
    }
    function money(n) { return '$' + (Number(n) || 0).toFixed(2); }
    function toast(msg, type) { if (typeof showToast === 'function') showToast(esc(msg), type || 'success'); }
    function el(html) { var d = document.createElement('div'); d.innerHTML = html.trim(); return d.firstChild; }
    function emailOk(e) { return /^[^\s@]+@[^\s@]+\.[^\s@]{2,}$/.test(e); }
    function phoneOk(p) { return String(p).replace(/\D/g, '').length >= 8; }
    function passOk(p) { return p.length >= 8 && /[A-Za-z]/.test(p) && /\d/.test(p); }
    function uid() { return 'u' + Date.now().toString(36) + Math.random().toString(36).slice(2, 8); }

    /* ---------- password hashing (PBKDF2 via Web Crypto) ---------- */
    function hashPw(pw, salt) {
        if (!(window.crypto && crypto.subtle && window.TextEncoder)) {
            return Promise.reject(new Error('Secure sign-in needs https or localhost.'));
        }
        var enc = new TextEncoder();
        return crypto.subtle.importKey('raw', enc.encode(pw), 'PBKDF2', false, ['deriveBits']).then(function (k) {
            return crypto.subtle.deriveBits({ name: 'PBKDF2', salt: enc.encode(salt), iterations: 100000, hash: 'SHA-256' }, k, 256);
        }).then(function (b) {
            return Array.prototype.map.call(new Uint8Array(b), function (x) { return ('0' + x.toString(16)).slice(-2); }).join('');
        });
    }
    function newSalt() {
        var a = new Uint8Array(16);
        if (window.crypto && crypto.getRandomValues) crypto.getRandomValues(a);
        return Array.prototype.map.call(a, function (x) { return ('0' + x.toString(16)).slice(-2); }).join('');
    }

    /* ---------- users & session ---------- */
    var USERS = 'rz_users', SESSION = 'rz_session';
    function users() { return store.get(USERS, []); }
    function saveUsers(u) { store.set(USERS, u); }
    function findByEmail(e) {
        e = String(e).trim().toLowerCase();
        return users().filter(function (u) { return u.email === e; })[0] || null;
    }
    function sessionId() {
        try { return sessionStorage.getItem(SESSION) || localStorage.getItem(SESSION) || null; } catch (e) { return null; }
    }
    function setSession(id, remember) {
        try {
            sessionStorage.removeItem(SESSION); localStorage.removeItem(SESSION);
            (remember ? localStorage : sessionStorage).setItem(SESSION, id);
        } catch (e) {}
    }
    function clearSession() { try { sessionStorage.removeItem(SESSION); localStorage.removeItem(SESSION); } catch (e) {} }
    function me() {
        var id = sessionId();
        if (!id) return null;
        return users().filter(function (u) { return u.id === id; })[0] || null;
    }
    function updateUser(u) {
        var all = users().map(function (x) { return x.id === u.id ? u : x; });
        saveUsers(all);
    }
    function owner() { var u = me(); return u ? u.id : 'guest'; }

    /* per-user storage keys used by the existing address book */
    window.RZ = { addrKey: function () { return 'rz_addr_' + owner(); } };

    /* ---------- product data from the menu cards ---------- */
    var PRODUCTS = {};
    var PRODUCT_LIST = [];
    qa('.mcard').forEach(function (c, i) {
        var p = {
            idx: i,
            card: c,
            title: c.getAttribute('data-title'),
            cat: c.getAttribute('data-cat'),
            img: c.getAttribute('data-img'),
            price: parseFloat(String(c.getAttribute('data-price')).replace('$', '')) || 0,
            old: c.getAttribute('data-old') || '',
            rating: parseFloat(c.getAttribute('data-rating')) || 0,
            reviews: parseInt(c.getAttribute('data-reviews'), 10) || 0,
            desc: c.getAttribute('data-desc') || '',
            tags: (c.getAttribute('data-tags') || '').toLowerCase()
        };
        PRODUCTS[p.title] = p;
        PRODUCT_LIST.push(p);
    });

    function addToCart(title) {
        var p = PRODUCTS[title];
        if (!p) return;
        var ex = cartItems.filter(function (it) {
            return it.title === title && (!it.size || it.size === 'Regular') && (!it.spice || it.spice === 'Mild') && (!it.extras || !it.extras.length);
        })[0];
        if (ex) ex.qty++;
        else cartItems.push({ img: p.img, title: p.title, price: p.price, qty: 1, size: 'Regular', spice: 'Mild', extras: [] });
        updateCartUI();
        toast('Added to cart!');
    }

    /* ---------- modal helper ---------- */
    var openModals = [];
    function makeModal(id, inner, cls) {
        var ov = el('<div class="rzov" id="' + id + '" role="dialog" aria-modal="true"><div class="rzbox ' + (cls || '') + '">' +
            '<button class="rzx" aria-label="Close"><i class="fas fa-times"></i></button>' + inner + '</div></div>');
        document.body.appendChild(ov);
        q('.rzx', ov).addEventListener('click', function () { closeModal(ov); });
        ov.addEventListener('mousedown', function (e) { if (e.target === ov) closeModal(ov); });
        return ov;
    }
    function openModal(ov) {
        if (openModals.indexOf(ov) === -1) openModals.push(ov);
        ov.classList.add('open');
        document.body.style.overflow = 'hidden';
        var f = q('input:not([type=hidden]),button.rzfocus', ov);
        if (f && window.innerWidth > 700) setTimeout(function () { f.focus(); }, 50);
    }
    function closeModal(ov) {
        ov.classList.remove('open');
        openModals = openModals.filter(function (m) { return m !== ov; });
        if (!openModals.length) document.body.style.overflow = '';
    }
    document.addEventListener('keydown', function (e) {
        if (e.key === 'Escape' && openModals.length) closeModal(openModals[openModals.length - 1]);
    });

    /* ============================================================
       NAV BUTTONS
       ============================================================ */
    var navRight = q('#navmenu .d-flex.align-items-center');
    var wishBtn = el('<button class="rzbtn" id="rzWish" title="Wishlist" aria-label="Wishlist"><i class="far fa-heart"></i><span class="rzcnt" id="rzWishCnt">0</span></button>');
    var acctBtn = el('<button class="rzbtn rzacct" id="rzAcct" title="Account" aria-label="Account"><i class="far fa-user"></i><span class="rzlbl" id="rzAcctLbl">Sign in</span></button>');
    if (navRight) {
        navRight.insertBefore(acctBtn, navRight.firstChild);
        navRight.insertBefore(wishBtn, navRight.firstChild);
    }

    /* ============================================================
       WISHLIST
       ============================================================ */
    function wishKey() { return 'rz_wish_' + owner(); }
    function wishList() { return store.get(wishKey(), []); }
    function inWish(t) { return wishList().indexOf(t) > -1; }
    function setWish(t, on) {
        var w = wishList().filter(function (x) { return x !== t; });
        if (on) w.unshift(t);
        store.set(wishKey(), w);
        refreshWishUI();
    }
    function paintHeart(card, on) {
        var btn = q('.mhrt', card), ico = btn && q('i', btn);
        if (!ico) return;
        ico.classList.toggle('fas', on);
        ico.classList.toggle('far', !on);
        btn.style.color = on ? 'var(--primary)' : '#ccc';
    }
    function refreshWishUI() {
        var w = wishList();
        q('#rzWishCnt').textContent = w.length;
        q('#rzWishCnt').style.display = w.length ? 'flex' : 'none';
        PRODUCT_LIST.forEach(function (p) { paintHeart(p.card, w.indexOf(p.title) > -1); });
    }
    qa('.mhrt').forEach(function (btn) {
        btn.addEventListener('click', function (e) {
            e.stopPropagation();
            var card = btn.closest('.mcard'), title = card.getAttribute('data-title');
            var on = !inWish(title);
            setWish(title, on);
            if (on) { btn.classList.add('heart-wish'); setTimeout(function () { btn.classList.remove('heart-wish'); }, 500); }
            toast(on ? 'Added to wishlist!' : 'Removed from wishlist', on ? 'success' : 'warn');
        });
    });
    wishBtn.addEventListener('click', function () { openAccount('wish'); });

    /* ============================================================
       AUTH MODAL
       ============================================================ */
    var authModal = makeModal('rzAuth',
        '<div class="rzhead"><div class="rzlogo"><i class="fas fa-utensils"></i></div>' +
        '<h3 id="rzAuthTitle">Welcome back</h3><p id="rzAuthSub">Sign in to track orders and save favourites.</p></div>' +
        '<div class="rztabs" id="rzAuthTabs"><button class="rztab active" data-t="login">Sign in</button><button class="rztab" data-t="signup">Create account</button></div>' +

        '<form id="rzLogin" class="rzform" novalidate>' +
        '<label>Email<input type="email" id="lgEmail" autocomplete="email" placeholder="you@example.com"></label>' +
        '<label>Password<span class="rzpw"><input type="password" id="lgPass" autocomplete="current-password" placeholder="Your password"><button type="button" class="rzeye" aria-label="Show password"><i class="far fa-eye"></i></button></span></label>' +
        '<div class="rzrow"><label class="rzchk"><input type="checkbox" id="lgRemember" checked> Remember me</label><a href="#" id="rzForgot">Forgot password?</a></div>' +
        '<div class="rzerr" id="lgErr" role="alert"></div>' +
        '<button class="rzprimary" type="submit" id="lgBtn">Sign in</button></form>' +

        '<form id="rzSignup" class="rzform" style="display:none" novalidate>' +
        '<label>Full name<input type="text" id="suName" autocomplete="name" placeholder="Jane Doe"></label>' +
        '<label>Email<input type="email" id="suEmail" autocomplete="email" placeholder="you@example.com"></label>' +
        '<label>Phone<input type="tel" id="suPhone" autocomplete="tel" placeholder="+1 555 000 0000"></label>' +
        '<label>Password<span class="rzpw"><input type="password" id="suPass" autocomplete="new-password" placeholder="8+ characters, letters and a number"><button type="button" class="rzeye" aria-label="Show password"><i class="far fa-eye"></i></button></span></label>' +
        '<div class="rzmeter"><span id="suMeter"></span></div>' +
        '<label>Confirm password<input type="password" id="suPass2" autocomplete="new-password" placeholder="Repeat password"></label>' +
        '<label class="rzchk"><input type="checkbox" id="suTerms"> I agree to the <a href="#" data-legal="terms">Terms</a> and <a href="#" data-legal="privacy">Privacy Policy</a></label>' +
        '<div class="rzerr" id="suErr" role="alert"></div>' +
        '<button class="rzprimary" type="submit" id="suBtn">Create account</button></form>' +

        '<form id="rzForgotForm" class="rzform" style="display:none" novalidate>' +
        '<p class="rzhint">Enter your account email, then choose a new password.<br><small>Demo store: no email is sent, the reset happens on this device.</small></p>' +
        '<label>Email<input type="email" id="fpEmail" placeholder="you@example.com"></label>' +
        '<label>New password<span class="rzpw"><input type="password" id="fpPass" autocomplete="new-password" placeholder="8+ characters, letters and a number"><button type="button" class="rzeye" aria-label="Show password"><i class="far fa-eye"></i></button></span></label>' +
        '<div class="rzerr" id="fpErr" role="alert"></div>' +
        '<button class="rzprimary" type="submit">Reset password</button>' +
        '<a href="#" class="rzback" id="fpBack">&larr; Back to sign in</a></form>',
        'rzauth');

    var afterAuth = null;
    function showAuth(tab, cb) {
        afterAuth = cb || null;
        switchAuth(tab || 'login');
        openModal(authModal);
    }
    function switchAuth(t) {
        qa('.rztab', authModal).forEach(function (b) { b.classList.toggle('active', b.getAttribute('data-t') === t); });
        q('#rzLogin').style.display = t === 'login' ? '' : 'none';
        q('#rzSignup').style.display = t === 'signup' ? '' : 'none';
        q('#rzForgotForm').style.display = t === 'forgot' ? '' : 'none';
        q('#rzAuthTabs').style.display = t === 'forgot' ? 'none' : '';
        q('#rzAuthTitle').textContent = t === 'signup' ? 'Create your account' : t === 'forgot' ? 'Reset password' : 'Welcome back';
        q('#rzAuthSub').textContent = t === 'signup' ? 'Join for faster checkout, order history and a wishlist.' :
            t === 'forgot' ? 'We will get you back in.' : 'Sign in to track orders and save favourites.';
        qa('.rzerr', authModal).forEach(function (e) { e.textContent = ''; });
    }
    qa('.rztab', authModal).forEach(function (b) {
        b.addEventListener('click', function () { switchAuth(b.getAttribute('data-t')); });
    });
    q('#rzForgot').addEventListener('click', function (e) { e.preventDefault(); switchAuth('forgot'); });
    q('#fpBack').addEventListener('click', function (e) { e.preventDefault(); switchAuth('login'); });
    qa('.rzeye', authModal).forEach(function (b) {
        b.addEventListener('click', function () {
            var inp = q('input', b.parentNode), show = inp.type === 'password';
            inp.type = show ? 'text' : 'password';
            q('i', b).className = show ? 'far fa-eye-slash' : 'far fa-eye';
        });
    });
    q('#suPass').addEventListener('input', function () {
        var p = this.value, s = 0;
        if (p.length >= 8) s++;
        if (/[A-Z]/.test(p) && /[a-z]/.test(p)) s++;
        if (/\d/.test(p)) s++;
        if (/[^A-Za-z0-9]/.test(p) || p.length >= 12) s++;
        var m = q('#suMeter');
        m.style.width = (s * 25) + '%';
        m.style.background = ['#ddd', '#ef4444', '#f59e0b', '#84cc16', '#22c55e'][s];
    });

    var fails = {};
    function setBusy(btn, busy, label) { btn.disabled = busy; btn.textContent = busy ? 'Please wait...' : label; }

    q('#rzLogin').addEventListener('submit', function (e) {
        e.preventDefault();
        var email = q('#lgEmail').value.trim().toLowerCase(), pass = q('#lgPass').value, err = q('#lgErr'), btn = q('#lgBtn');
        err.textContent = '';
        if (!emailOk(email)) { err.textContent = 'Enter a valid email address.'; return; }
        if (!pass) { err.textContent = 'Enter your password.'; return; }
        var f = fails[email];
        if (f && f.until > Date.now()) { err.textContent = 'Too many attempts. Try again in ' + Math.ceil((f.until - Date.now()) / 1000) + 's.'; return; }
        var u = findByEmail(email);
        setBusy(btn, true);
        (u ? hashPw(pass, u.salt) : Promise.resolve(null)).then(function (h) {
            if (!u || h !== u.hash) {
                f = fails[email] = fails[email] || { n: 0, until: 0 };
                f.n++;
                if (f.n >= 5) { f.until = Date.now() + 30000; f.n = 0; }
                err.textContent = 'Incorrect email or password.';
                return;
            }
            delete fails[email];
            signIn(u, q('#lgRemember').checked);
            q('#lgPass').value = '';
        }).catch(function (x) { err.textContent = x.message; }).then(function () { setBusy(btn, false, 'Sign in'); });
    });

    q('#rzSignup').addEventListener('submit', function (e) {
        e.preventDefault();
        var name = q('#suName').value.trim(), email = q('#suEmail').value.trim().toLowerCase(), phone = q('#suPhone').value.trim(),
            p1 = q('#suPass').value, p2 = q('#suPass2').value, err = q('#suErr'), btn = q('#suBtn');
        err.textContent = '';
        if (name.length < 2) { err.textContent = 'Please enter your full name.'; return; }
        if (!emailOk(email)) { err.textContent = 'Enter a valid email address.'; return; }
        if (!phoneOk(phone)) { err.textContent = 'Enter a valid phone number.'; return; }
        if (!passOk(p1)) { err.textContent = 'Password needs 8+ characters with letters and a number.'; return; }
        if (p1 !== p2) { err.textContent = 'Passwords do not match.'; return; }
        if (!q('#suTerms').checked) { err.textContent = 'Please accept the Terms and Privacy Policy.'; return; }
        if (findByEmail(email)) { err.textContent = 'An account with this email already exists. Try signing in.'; return; }
        var salt = newSalt();
        setBusy(btn, true);
        hashPw(p1, salt).then(function (h) {
            var u = { id: uid(), name: name, email: email, phone: phone, salt: salt, hash: h, created: Date.now() };
            var all = users(); all.push(u); saveUsers(all);
            qa('input', q('#rzSignup')).forEach(function (i) { if (i.type !== 'checkbox') i.value = ''; else i.checked = false; });
            signIn(u, true, true);
        }).catch(function (x) { err.textContent = x.message; }).then(function () { setBusy(btn, false, 'Create account'); });
    });

    q('#rzForgotForm').addEventListener('submit', function (e) {
        e.preventDefault();
        var email = q('#fpEmail').value.trim().toLowerCase(), p = q('#fpPass').value, err = q('#fpErr');
        err.textContent = '';
        var u = findByEmail(email);
        if (!emailOk(email)) { err.textContent = 'Enter a valid email address.'; return; }
        if (!passOk(p)) { err.textContent = 'Password needs 8+ characters with letters and a number.'; return; }
        if (!u) { err.textContent = 'No account found with that email.'; return; }
        var salt = newSalt();
        hashPw(p, salt).then(function (h) {
            u.salt = salt; u.hash = h; updateUser(u);
            q('#fpPass').value = '';
            toast('Password updated. Please sign in.');
            q('#lgEmail').value = email;
            switchAuth('login');
        }).catch(function (x) { err.textContent = x.message; });
    });

    function signIn(u, remember, isNew) {
        var guestWish = store.get('rz_wish_guest', []);
        setSession(u.id, remember);
        if (guestWish.length) {
            var merged = store.get('rz_wish_' + u.id, []);
            guestWish.forEach(function (t) { if (merged.indexOf(t) === -1) merged.push(t); });
            store.set('rz_wish_' + u.id, merged);
            store.del('rz_wish_guest');
        }
        /* guest addresses move into the new account */
        var ga = store.get('rz_addr_guest', []) || [];
        if (ga.length) {
            var ua = store.get('rz_addr_' + u.id, []);
            ga.forEach(function (a) { ua.push(a); });
            store.set('rz_addr_' + u.id, ua);
            store.del('rz_addr_guest');
        }
        refreshAuthUI();
        closeModal(authModal);
        toast(isNew ? 'Welcome, ' + u.name.split(' ')[0] + '! Your account is ready.' : 'Welcome back, ' + u.name.split(' ')[0] + '!');
        var cb = afterAuth; afterAuth = null;
        if (cb) cb();
    }
    function signOut() {
        clearSession();
        refreshAuthUI();
        closeModal(acctModal);
        toast('You have been signed out.', 'warn');
    }

    function refreshAuthUI() {
        var u = me();
        q('#rzAcctLbl').textContent = u ? u.name.split(' ')[0] : 'Sign in';
        acctBtn.classList.toggle('on', !!u);
        q('i', acctBtn).className = u ? 'fas fa-user' : 'far fa-user';
        addrBook = loadAddrBook();
        selAddr = null;
        renderAddr();
        refreshWishUI();
        fillCheckoutContact();
        updateGuestNotice();
        updateOrdersBtn();
        if (acctModal && acctModal.classList.contains('open')) renderAccount();
    }
    acctBtn.addEventListener('click', function () {
        if (me()) openAccount('profile'); else showAuth('login');
    });

    /* ============================================================
       ACCOUNT MODAL
       ============================================================ */
    var acctTab = 'profile';
    var acctModal = makeModal('rzAcctModal',
        '<div class="rzhead small"><div class="rzavatar" id="acAvatar"></div><div><h3 id="acName"></h3><p id="acMeta"></p></div></div>' +
        '<div class="rztabs scroll" id="acTabs">' +
        '<button class="rztab" data-t="profile"><i class="far fa-user"></i> Profile</button>' +
        '<button class="rztab" data-t="orders"><i class="fas fa-receipt"></i> Orders</button>' +
        '<button class="rztab" data-t="wish"><i class="far fa-heart"></i> Wishlist</button>' +
        '<button class="rztab" data-t="addr"><i class="fas fa-map-marker-alt"></i> Addresses</button>' +
        '<button class="rztab" data-t="sec"><i class="fas fa-shield-halved"></i> Security</button></div>' +
        '<div class="rzpane" id="acPane"></div>', 'rzacct');

    qa('.rztab', acctModal).forEach(function (b) {
        b.addEventListener('click', function () {
            var t = b.getAttribute('data-t');
            if (!me() && t !== 'wish') { showAuth('login', function () { openAccount(t); }); return; }
            acctTab = t; renderAccount();
        });
    });

    function openAccount(tab) {
        if (!me() && tab !== 'wish') { showAuth('login', function () { openAccount(tab); }); return; }
        acctTab = tab || 'profile';
        renderAccount();
        openModal(acctModal);
    }

    function myOrders() {
        var id = owner();
        return store.get('yussif_orders', []).filter(function (o) { return (o.uid || 'guest') === id; });
    }
    function points() {
        return Math.floor(myOrders().filter(function (o) { return !o.cancelled; })
            .reduce(function (a, o) { return a + (parseFloat(String(o.total).replace('$', '')) || 0); }, 0));
    }

    function renderAccount() {
        var u = me();
        q('#acAvatar').textContent = u ? u.name.charAt(0).toUpperCase() : '?';
        q('#acName').textContent = u ? u.name : 'Guest';
        q('#acMeta').innerHTML = u ? esc(u.email) + ' &middot; <i class="fas fa-star" style="color:#f59e0b"></i> ' + points() + ' points' : 'Sign in to keep your favourites safe';
        qa('.rztab', acctModal).forEach(function (b) { b.classList.toggle('active', b.getAttribute('data-t') === acctTab); });
        var pane = q('#acPane');
        ({ profile: paneProfile, orders: paneOrders, wish: paneWish, addr: paneAddr, sec: paneSec })[acctTab](pane, u);
    }

    function paneProfile(pane, u) {
        pane.innerHTML = '<form class="rzform" id="pfForm" novalidate>' +
            '<label>Full name<input id="pfName" value="' + esc(u.name) + '"></label>' +
            '<label>Email<input id="pfEmail" type="email" value="' + esc(u.email) + '"></label>' +
            '<label>Phone<input id="pfPhone" type="tel" value="' + esc(u.phone || '') + '"></label>' +
            '<div class="rzerr" id="pfErr" role="alert"></div>' +
            '<button class="rzprimary" type="submit">Save changes</button></form>' +
            '<p class="rzhint">Member since ' + new Date(u.created).toLocaleDateString() + '. Earn 1 point for every $1 you spend.</p>' +
            '<button class="rzghost" id="pfOut"><i class="fas fa-sign-out-alt"></i> Sign out</button>';
        q('#pfOut', pane).addEventListener('click', signOut);
        q('#pfForm', pane).addEventListener('submit', function (e) {
            e.preventDefault();
            var name = q('#pfName').value.trim(), email = q('#pfEmail').value.trim().toLowerCase(), phone = q('#pfPhone').value.trim(), err = q('#pfErr');
            if (name.length < 2) { err.textContent = 'Please enter your name.'; return; }
            if (!emailOk(email)) { err.textContent = 'Enter a valid email address.'; return; }
            if (phone && !phoneOk(phone)) { err.textContent = 'Enter a valid phone number.'; return; }
            var other = findByEmail(email);
            if (other && other.id !== u.id) { err.textContent = 'That email is already used by another account.'; return; }
            u.name = name; u.email = email; u.phone = phone; updateUser(u);
            refreshAuthUI();
            renderAccount();
            toast('Profile updated.');
        });
    }

    /* ----- order status is derived from elapsed time (demo kitchen) ----- */
    var STAGES = ['Order Received', 'Preparing', 'Out for Delivery', 'Delivered'];
    function statusOf(o) {
        if (o.cancelled) return 'Cancelled';
        if (!o.created) return o.status || STAGES[0];
        var m = (Date.now() - o.created) / 60000;
        return m < 1 ? STAGES[0] : m < 3 ? STAGES[1] : m < 6 ? STAGES[2] : STAGES[3];
    }
    function statusCls(s) { return s === 'Delivered' ? 'ok' : s === 'Cancelled' ? 'bad' : 'live'; }

    function paneOrders(pane) {
        var list = myOrders();
        if (!list.length) {
            pane.innerHTML = '<div class="rzempty"><i class="fas fa-box-open"></i><p>No orders yet</p><a href="#menu" class="rzprimary rzlink">Browse the menu</a></div>';
            q('.rzlink', pane).addEventListener('click', function () { closeModal(acctModal); });
            return;
        }
        pane.innerHTML = list.map(function (o) {
            var s = statusOf(o);
            return '<div class="rzorder" data-id="' + esc(o.id) + '">' +
                '<div class="rzohead"><strong>#' + esc(o.id) + '</strong><span class="rzchip ' + statusCls(s) + '">' + esc(s) + '</span></div>' +
                '<div class="rzodate">' + esc(o.date || '') + ' &middot; ' + esc(o.method) + '</div>' +
                '<div class="rzoitems">' + (o.items || []).map(esc).join('<br>') + '</div>' +
                '<div class="rzofoot"><strong>' + esc(o.total) + '</strong><div class="rzobtns">' +
                (o.lines ? '<button data-a="reorder">Reorder</button><button data-a="receipt">Receipt</button>' : '') +
                (s === STAGES[0] ? '<button data-a="cancel" class="danger">Cancel</button>' : '') +
                '</div></div></div>';
        }).join('');
    }
    acctModal.addEventListener('click', function (e) {
        var b = e.target.closest('[data-a]');
        if (!b) return;
        var a = b.getAttribute('data-a');
        var row = b.closest('[data-id]'), id = row && row.getAttribute('data-id');
        if (a === 'reorder' || a === 'receipt' || a === 'cancel') {
            var all = store.get('yussif_orders', []), o = all.filter(function (x) { return x.id === id; })[0];
            if (!o) return;
            if (a === 'reorder') {
                (o.lines || []).forEach(function (l) {
                    var ex = cartItems.filter(function (it) {
                        return it.title === l.title && it.size === l.size && it.spice === l.spice && JSON.stringify(it.extras || []) === JSON.stringify(l.extras || []);
                    })[0];
                    if (ex) ex.qty += l.qty; else cartItems.push({ img: l.img, title: l.title, price: l.price, qty: l.qty, size: l.size, spice: l.spice, extras: (l.extras || []).slice() });
                });
                updateCartUI(); closeModal(acctModal);
                toast('Items added to your cart.');
                var cb = document.getElementById('cartBtn'); if (cb && !cartOpen) cb.click();
            } else if (a === 'receipt') {
                printReceipt(o);
            } else if (statusOf(o) === STAGES[0] && confirm('Cancel order #' + o.id + '?')) {
                o.cancelled = true; store.set('yussif_orders', all);
                toast('Order cancelled.', 'warn'); renderAccount();
            }
        } else if (a === 'wcart') {
            addToCart(b.getAttribute('data-t'));
        } else if (a === 'wdel') {
            var t = b.getAttribute('data-t'); setWish(t, false); renderAccount();
        } else if (a === 'wview') {
            var p = PRODUCTS[b.getAttribute('data-t')]; if (p) { closeModal(acctModal); openMenuPop(p.card); }
        } else if (a === 'adel') {
            var i = parseInt(b.getAttribute('data-i'), 10);
            addrBook.splice(i, 1); saveAddrBook(); selAddr = null; renderAddr(); renderAccount();
            toast('Address removed.', 'warn');
        }
    });

    function printReceipt(o) {
        var rows = (o.lines || []).map(function (l) {
            var opt = [l.size !== 'Regular' ? l.size : '', l.spice !== 'Mild' ? l.spice : ''].concat(l.extras || []).filter(Boolean).join(', ');
            return '<tr><td>' + esc(l.qty) + ' x ' + esc(l.title) + (opt ? '<br><small>' + esc(opt) + '</small>' : '') + '</td><td style="text-align:right">' + money(l.price * l.qty) + '</td></tr>';
        }).join('');
        var w = window.open('', '_blank', 'width=520,height=720');
        if (!w) { toast('Allow pop-ups to print the receipt.', 'warn'); return; }
        w.document.write('<!doctype html><title>Receipt ' + esc(o.id) + '</title><body style="font-family:Arial,sans-serif;max-width:420px;margin:24px auto;color:#222">' +
            '<h2 style="margin:0">Richy\'s Eat</h2><p style="margin:2px 0 14px;color:#666">42 Flavor Street, NY &middot; 0531629095</p>' +
            '<p><strong>Order #' + esc(o.id) + '</strong><br>' + esc(o.date) + '<br>Payment: ' + esc(o.method) + '</p>' +
            (o.address ? '<p>Deliver to:<br>' + esc(o.address) + '</p>' : '') +
            '<table style="width:100%;border-collapse:collapse" cellpadding="6">' + rows +
            '<tr><td colspan="2"><hr></td></tr>' +
            '<tr><td>Subtotal</td><td style="text-align:right">' + money(o.subtotal) + '</td></tr>' +
            (o.discount ? '<tr><td>Discount' + (o.promo ? ' (' + esc(o.promo) + ')' : '') + '</td><td style="text-align:right">-' + money(o.discount) + '</td></tr>' : '') +
            '<tr><td>Delivery</td><td style="text-align:right">' + (o.delivery ? money(o.delivery) : 'Free') + '</td></tr>' +
            '<tr><td><strong>Total</strong></td><td style="text-align:right"><strong>' + esc(o.total) + '</strong></td></tr></table>' +
            '<p style="color:#888;font-size:12px;margin-top:24px">Thank you for ordering!</p>' +
            '<script>window.onload=function(){window.print()}<\/script></body>');
        w.document.close();
    }

    function paneWish(pane) {
        var items = wishList().map(function (t) { return PRODUCTS[t]; }).filter(Boolean);
        if (!items.length) {
            pane.innerHTML = '<div class="rzempty"><i class="far fa-heart"></i><p>Your wishlist is empty</p><span>Tap the heart on any dish to save it here.</span></div>';
            return;
        }
        pane.innerHTML = items.map(function (p) {
            return '<div class="rzwish"><img src="' + esc(p.img) + '" alt=""><div class="rzwinfo"><strong>' + esc(p.title) + '</strong>' +
                '<span>' + esc(p.cat) + ' &middot; <i class="fas fa-star" style="color:#f59e0b"></i> ' + p.rating + '</span><b>' + money(p.price) + '</b></div>' +
                '<div class="rzwbtn"><button data-a="wcart" data-t="' + esc(p.title) + '" title="Add to cart"><i class="fas fa-cart-plus"></i></button>' +
                '<button data-a="wview" data-t="' + esc(p.title) + '" title="View"><i class="far fa-eye"></i></button>' +
                '<button data-a="wdel" data-t="' + esc(p.title) + '" class="danger" title="Remove"><i class="fas fa-trash-alt"></i></button></div></div>';
        }).join('');
    }

    function paneAddr(pane) {
        pane.innerHTML = (addrBook.length ? addrBook.map(function (a, i) {
            return '<div class="rzaddr"><div><strong>' + esc(a.addr) + '</strong><span>' + esc(a.city) + ', ' + esc(a.zip) + '</span></div>' +
                '<button data-a="adel" data-i="' + i + '" class="danger" title="Remove"><i class="fas fa-trash-alt"></i></button></div>';
        }).join('') : '<div class="rzempty"><i class="fas fa-map-marked-alt"></i><p>No saved addresses</p></div>') +
            '<form class="rzform" id="adForm" novalidate><h5>Add an address</h5>' +
            '<label>Street address<input id="adStreet"></label><div class="rzgrid2"><label>City<input id="adCity"></label><label>Zip code<input id="adZip"></label></div>' +
            '<div class="rzerr" id="adErr" role="alert"></div><button class="rzprimary" type="submit">Save address</button></form>';
        q('#adForm', pane).addEventListener('submit', function (e) {
            e.preventDefault();
            var a = q('#adStreet').value.trim(), c = q('#adCity').value.trim(), z = q('#adZip').value.trim();
            if (!a || !c || !z) { q('#adErr').textContent = 'Please fill in all address fields.'; return; }
            addrBook.push({ addr: a, city: c, zip: z }); saveAddrBook(); renderAddr(); renderAccount();
            toast('Address saved.');
        });
    }

    function paneSec(pane, u) {
        pane.innerHTML = '<form class="rzform" id="cpForm" novalidate><h5>Change password</h5>' +
            '<label>Current password<input type="password" id="cpOld" autocomplete="current-password"></label>' +
            '<label>New password<input type="password" id="cpNew" autocomplete="new-password" placeholder="8+ characters, letters and a number"></label>' +
            '<div class="rzerr" id="cpErr" role="alert"></div><button class="rzprimary" type="submit">Update password</button></form>' +
            '<form class="rzform danger-zone" id="dlForm" novalidate><h5>Delete account</h5>' +
            '<p class="rzhint">This removes your profile, wishlist, saved addresses and order history from this device.</p>' +
            '<label>Confirm with your password<input type="password" id="dlPass" autocomplete="current-password"></label>' +
            '<div class="rzerr" id="dlErr" role="alert"></div><button class="rzprimary red" type="submit">Delete my account</button></form>';
        q('#cpForm', pane).addEventListener('submit', function (e) {
            e.preventDefault();
            var o = q('#cpOld').value, n = q('#cpNew').value, err = q('#cpErr');
            if (!passOk(n)) { err.textContent = 'New password needs 8+ characters with letters and a number.'; return; }
            hashPw(o, u.salt).then(function (h) {
                if (h !== u.hash) { err.textContent = 'Current password is incorrect.'; return; }
                var salt = newSalt();
                return hashPw(n, salt).then(function (nh) {
                    u.salt = salt; u.hash = nh; updateUser(u);
                    q('#cpOld').value = ''; q('#cpNew').value = '';
                    toast('Password updated.');
                });
            }).catch(function (x) { err.textContent = x.message; });
        });
        q('#dlForm', pane).addEventListener('submit', function (e) {
            e.preventDefault();
            var err = q('#dlErr');
            hashPw(q('#dlPass').value, u.salt).then(function (h) {
                if (h !== u.hash) { err.textContent = 'Password is incorrect.'; return; }
                if (!confirm('Permanently delete your account? This cannot be undone.')) return;
                saveUsers(users().filter(function (x) { return x.id !== u.id; }));
                store.del('rz_wish_' + u.id); store.del('rz_addr_' + u.id);
                store.set('yussif_orders', store.get('yussif_orders', []).filter(function (o) { return o.uid !== u.id; }));
                clearSession(); refreshAuthUI(); closeModal(acctModal);
                toast('Your account has been deleted.', 'warn');
            }).catch(function (x) { err.textContent = x.message; });
        });
    }

    /* ============================================================
       MENU TOOLBAR: search, sort, price, vegetarian
       ============================================================ */
    var grid = q('#mgrid');
    if (grid) {
        var tools = el('<div class="shoptools">' +
            '<div class="stsearch"><i class="fas fa-search"></i><input id="stQ" type="search" placeholder="Search the menu..." aria-label="Search the menu"></div>' +
            '<select id="stSort" aria-label="Sort menu"><option value="feat">Featured</option><option value="plow">Price: low to high</option><option value="phigh">Price: high to low</option>' +
            '<option value="rate">Top rated</option><option value="pop">Most reviewed</option></select>' +
            '<label class="rzchk stveg"><input type="checkbox" id="stVeg"> Vegetarian</label>' +
            '<button id="stReset" class="rzghost" type="button">Reset</button></div>');
        grid.parentNode.insertBefore(tools, grid);
        var empty = el('<div id="stEmpty" class="rzempty" style="display:none"><i class="fas fa-search"></i><p>No dishes match your filters</p><span>Try a different search or reset the filters.</span></div>');
        grid.parentNode.insertBefore(empty, grid.nextSibling);

        var applyTools = function () {
            var text = q('#stQ').value.trim().toLowerCase(), veg = q('#stVeg').checked;
            PRODUCT_LIST.forEach(function (p) {
                var hay = (p.title + ' ' + p.cat + ' ' + p.desc + ' ' + p.tags).toLowerCase();
                var show = (!text || hay.indexOf(text) > -1) && (!veg || p.tags.indexOf('vegetarian') > -1);
                p.card.closest('.mwrap').classList.toggle('rz-hide', !show);
            });
            var s = q('#stSort').value, arr = PRODUCT_LIST.slice();
            arr.sort(function (a, b) {
                return s === 'plow' ? a.price - b.price : s === 'phigh' ? b.price - a.price :
                    s === 'rate' ? b.rating - a.rating : s === 'pop' ? b.reviews - a.reviews : a.idx - b.idx;
            });
            arr.forEach(function (p) { grid.appendChild(p.card.closest('.mwrap')); });
            updateEmpty();
        };
        var updateEmpty = function () {
            var vis = qa('.mwrap', grid).filter(function (w) { return !w.classList.contains('gone') && !w.classList.contains('rz-hide'); }).length;
            empty.style.display = vis ? 'none' : 'block';
        };
        ['input', 'change'].forEach(function (ev) {
            ['#stQ', '#stSort', '#stVeg'].forEach(function (s) { q(s).addEventListener(ev, applyTools); });
        });
        var pa = q('#priceApply');
        if (pa) pa.addEventListener('click', function () { setTimeout(updateEmpty, 450); });
        q('#stReset').addEventListener('click', function () {
            q('#stQ').value = ''; q('#stSort').value = 'feat'; q('#stVeg').checked = false;
            if (q('#priceMin')) q('#priceMin').value = '';
            if (q('#priceMax')) q('#priceMax').value = '';
            applyTools();
            if (typeof filterMenu === 'function') filterMenu('all');
        });
        var prevFilter = window.filterMenu;
        window.filterMenu = function (c) { prevFilter(c); setTimeout(updateEmpty, 0); };
    }

    /* ============================================================
       TOTALS: delivery fee is part of the total
       ============================================================ */
    function totals() {
        var sub = getCartSubtotal(), disc = getDiscountAmount(sub);
        var fee = (sub > 0 && sub < FREE_DELIVERY_THRESHOLD) ? 4.99 : 0;
        return { sub: sub, disc: disc, fee: fee, total: Math.max(0, sub - disc) + fee };
    }
    var prevUpdate = window.updateCartUI;
    window.updateCartUI = function () {
        prevUpdate();
        var t = totals();
        q('#csTotal').textContent = money(t.total);
        q('#ckAmount').textContent = money(t.total);
        q('#ckTotal').textContent = money(t.total);
        q('#mobAmount').value = money(t.total);
        var d = q('.cksumdel span:last-child');
        if (d) { d.textContent = t.fee ? money(t.fee) : 'Free'; d.style.color = t.fee ? '#888' : '#4ade80'; }
    };

    /* ============================================================
       CHECKOUT: contact details, validation, order placement
       ============================================================ */
    var ckBox = q('#checkoutModal .ckbox');
    var guestNote = el('<div class="rzguest" id="rzGuest"><i class="fas fa-user-circle"></i><div><strong>Sign in for faster checkout</strong>' +
        '<span>Save your details, track orders and keep a wishlist.</span></div>' +
        '<button type="button" id="gnLogin">Sign in</button></div>');
    var contact = el('<div class="ckcontact" id="ckContact"><div class="ckaddhdr"><i class="fas fa-user me-2"></i>Contact Details</div>' +
        '<div class="rzgrid2"><input type="text" class="fctrl" id="ckName" placeholder="Full name" autocomplete="name">' +
        '<input type="tel" class="fctrl" id="ckPhone" placeholder="Phone number" autocomplete="tel"></div>' +
        '<input type="email" class="fctrl" id="ckEmail" placeholder="Email (optional, for your receipt)" autocomplete="email" style="margin-top:8px"></div>');
    var demoNote = el('<div class="rzdemo"><i class="fas fa-info-circle"></i> Demo checkout: no real payment is taken and card details are never saved. Do not enter real card numbers.</div>');
    var ckAddrBlock = q('#ckAddress');
    if (ckBox && ckAddrBlock) {
        ckAddrBlock.parentNode.insertBefore(guestNote, ckAddrBlock);
        ckAddrBlock.parentNode.insertBefore(contact, ckAddrBlock);
        var paysel = q('.paysel', ckBox);
        paysel.parentNode.insertBefore(demoNote, paysel);
    }
    q('#cardCvv') && q('#cardCvv').setAttribute('maxlength', '4');
    q('#gnLogin') && q('#gnLogin').addEventListener('click', function () { showAuth('login'); });

    function updateGuestNotice() { var g = q('#rzGuest'); if (g) g.style.display = me() ? 'none' : 'flex'; }
    function fillCheckoutContact() {
        var u = me();
        if (!q('#ckName')) return;
        if (u) {
            if (!q('#ckName').value) q('#ckName').value = u.name;
            if (!q('#ckPhone').value) q('#ckPhone').value = u.phone || '';
            if (!q('#ckEmail').value) q('#ckEmail').value = u.email;
        } else {
            q('#ckName').value = ''; q('#ckPhone').value = ''; q('#ckEmail').value = '';
        }
    }

    function luhn(n) {
        var s = 0, alt = false;
        for (var i = n.length - 1; i >= 0; i--) {
            var d = parseInt(n.charAt(i), 10);
            if (alt) { d *= 2; if (d > 9) d -= 9; }
            s += d; alt = !alt;
        }
        return n.length >= 13 && s % 10 === 0;
    }
    function mark(id, bad) { var e = q('#' + id); if (e) e.classList.toggle('rzbad', !!bad); return bad; }

    function validateCheckout(method) {
        qa('.rzbad').forEach(function (e) { e.classList.remove('rzbad'); });
        var name = q('#ckName').value.trim(), phone = q('#ckPhone').value.trim(), email = q('#ckEmail').value.trim();
        if (mark('ckName', name.length < 2)) return 'Please enter your name.';
        if (mark('ckPhone', !phoneOk(phone))) return 'Please enter a valid phone number.';
        if (email && mark('ckEmail', !emailOk(email))) return 'That email address looks invalid.';
        var addr = deliveryAddress();
        if (!addr) return 'Please choose or add a delivery address.';
        if (q('#delTime').value === 'schedule') {
            var st = q('#schedTime').value;
            if (mark('schedTime', !st || new Date(st).getTime() < Date.now())) return 'Choose a future date and time for your delivery.';
        }
        if (method === 'mobile') {
            if (mark('mobPhone', !phoneOk(q('#mobPhone').value))) return 'Enter your mobile money phone number.';
            if (mark('mobPin', !/^\d{4}$/.test(q('#mobPin').value))) return 'Enter your 4-digit mobile money PIN.';
        } else if (method === 'bank') {
            if (mark('bankRef', q('#bankRef').value.trim().length < 4)) return 'Enter your bank transaction reference.';
            if (mark('bankName', q('#bankName').value.trim().length < 2)) return 'Enter the sender name.';
        } else if (method === 'card') {
            var num = q('#cardNum').value.replace(/\s/g, '');
            if (mark('cardNum', !luhn(num))) return 'That card number is not valid.';
            var m = q('#cardExp').value.match(/^(\d{2})\/(\d{2})$/), badExp = true;
            if (m) {
                var mo = +m[1], yr = 2000 + +m[2], now = new Date();
                badExp = mo < 1 || mo > 12 || (yr < now.getFullYear() || (yr === now.getFullYear() && mo < now.getMonth() + 1));
            }
            if (mark('cardExp', badExp)) return 'Card expiry date is invalid or expired.';
            if (mark('cardCvv', !/^\d{3,4}$/.test(q('#cardCvv').value))) return 'Enter the 3 or 4 digit security code.';
            if (mark('cardName', q('#cardName').value.trim().length < 2)) return 'Enter the name on the card.';
        }
        return '';
    }
    function deliveryAddress() {
        if (selAddr !== null && addrBook[selAddr]) { var a = addrBook[selAddr]; return a.addr + ', ' + a.city + ' ' + a.zip; }
        var c = q('#cashAddr'); return c && c.value.trim() ? c.value.trim() : '';
    }
    function notesText() {
        var ids = ['orderNotes', 'orderNotes2', 'orderNotes3', 'orderNotes4', 'cashNotes'];
        return ids.map(function (i) { var e = q('#' + i); return e ? e.value.trim() : ''; }).filter(Boolean).join(' | ');
    }
    var METHOD_NAMES = { mobile: 'Mobile Money', bank: 'Bank Transfer', card: 'Card', cash: 'Cash on Delivery' };

    function nextOrderId() {
        var n = store.get('rz_ordseq', 1041) + 1;
        store.set('rz_ordseq', n);
        return 'SB-' + n;
    }

    /* replace main.js pay handler with a validated one */
    var oldPay = q('#ckPay'), payBtn = oldPay.cloneNode(true);
    oldPay.parentNode.replaceChild(payBtn, oldPay);
    payBtn.addEventListener('click', function () {
        if (!cartItems.length) { toast('Your cart is empty!', 'err'); return; }
        var tab = q('.paytab.active'), method = tab ? tab.getAttribute('data-pay') : 'cash';
        var problem = validateCheckout(method);
        if (problem) { toast(problem, 'warn'); var b = q('.rzbad'); if (b) { b.scrollIntoView({ block: 'center', behavior: 'smooth' }); b.focus(); } return; }
        var t = totals(), orig = payBtn.innerHTML;
        payBtn.disabled = true;
        payBtn.innerHTML = '<i class="fas fa-spinner fa-spin"></i> Processing...';
        setTimeout(function () {
            var lines = cartItems.map(function (it) {
                return { title: it.title, img: it.img, price: it.price, qty: it.qty, size: it.size || 'Regular', spice: it.spice || 'Mild', extras: (it.extras || []).slice() };
            });
            var order = {
                id: nextOrderId(),
                uid: owner(),
                items: lines.map(function (l) { return l.qty + 'x ' + l.title + (l.size !== 'Regular' ? ' (' + l.size + ')' : ''); }),
                lines: lines,
                subtotal: t.sub, discount: t.disc, delivery: t.fee, promo: appliedPromo || '',
                total: money(t.total),
                method: METHOD_NAMES[method] || method,
                address: deliveryAddress(),
                contact: { name: q('#ckName').value.trim(), phone: q('#ckPhone').value.trim(), email: q('#ckEmail').value.trim() },
                notes: notesText(),
                when: q('#delTime').value === 'schedule' ? q('#schedTime').value : q('#delTime').selectedOptions[0].textContent,
                date: new Date().toLocaleString(),
                created: Date.now()
            };
            var all = store.get('yussif_orders', []);
            all.unshift(order);
            store.set('yussif_orders', all.slice(0, 100));

            /* never keep payment details */
            ['mobPin', 'mobPhone', 'bankRef', 'bankName', 'cardNum', 'cardExp', 'cardCvv', 'cardName', 'orderNotes', 'orderNotes2', 'orderNotes3', 'orderNotes4', 'cashNotes'].forEach(function (i) {
                var e = q('#' + i); if (e) e.value = '';
            });
            payBtn.disabled = false; payBtn.innerHTML = orig;
            closeCheckout();
            openTracking(method);
            q('#tmId').textContent = order.id.replace('SB-', '');
            cartItems = []; appliedPromo = null;
            updateCartUI();
            cartOpen = false; cartSide.classList.remove('open'); csCvr.classList.remove('show');
            promoMsg.textContent = ''; promoMsg.className = 'cspmsg'; promoInput.value = '';
            document.body.style.overflow = 'hidden';
            updateOrdersBtn();
            toast('Order placed successfully!');
        }, 1600);
    });

    /* open checkout: refresh contact + notice */
    csCheck.addEventListener('click', function () { fillCheckoutContact(); updateGuestNotice(); });

    /* order history modal (legacy) only lists this user's orders */
    window.renderOrders = function () {
        if (!ohList) return;
        var orders = myOrders();
        if (!orders.length) { ohList.innerHTML = '<div class="ohempty"><i class="fas fa-box-open"></i><p>No orders yet</p></div>'; return; }
        ohList.innerHTML = '';
        orders.forEach(function (o) {
            var item = document.createElement('div');
            item.className = 'ohitem';
            item.innerHTML = '<div class="ohid">Order #' + esc(o.id) + ' <span class="ohstatus">' + esc(statusOf(o)) + '</span></div>' +
                '<div class="ohitems">' + (o.items || []).map(esc).join('<br/>') + '</div>' +
                '<div class="ohtotal">' + esc(o.total) + ' <span style="color:#888;font-weight:400;font-size:0.75rem;">via ' + esc(o.method) + '</span></div>';
            ohList.appendChild(item);
        });
    };
    window.updateOrdersBtn = function () {
        var b = document.getElementById('csOrders');
        if (b) b.style.display = myOrders().length ? 'block' : 'none';
    };
    window.saveOrder = function () { /* superseded by the validated checkout above */ };

    /* ============================================================
       REVIEWS: remember submitted reviews, prefill name
       ============================================================ */
    var savedReviews = store.get('rz_reviews', {});
    Object.keys(savedReviews).forEach(function (t) {
        reviewData[t] = (savedReviews[t] || []).concat(reviewData[t] || []);
    });
    var revSnap = null, revBtn = q('#mpRevBtn');
    revBtn.addEventListener('click', function () {
        revSnap = { t: q('#mpTitle').textContent, n: q('#mpRevName').value.trim(), b: q('#mpRevBody').value.trim() };
    }, true);
    revBtn.addEventListener('click', function () {
        if (!revSnap) return;
        var list = reviewData[revSnap.t], top = list && list[0];
        if (top && top.name === revSnap.n && top.text === revSnap.b) {
            var s = store.get('rz_reviews', {}); s[revSnap.t] = s[revSnap.t] || [];
            s[revSnap.t].unshift(top); store.set('rz_reviews', s);
        }
        revSnap = null;
    });
    var prevOpen = window.openMenuPop;
    window.openMenuPop = function (card) {
        prevOpen(card);
        var u = me(), n = q('#mpRevName');
        if (u && n && !n.value) n.value = u.name;
    };

    /* ============================================================
       LEGAL / HELP MODALS
       ============================================================ */
    var LEGAL = {
        privacy: ['Privacy Policy', '<p>We collect only what we need to take and deliver your order: name, phone, email and delivery address. Account and order data in this demo store are kept in your own browser and are not sent to a server.</p>' +
            '<p>If you contact us through the reservation or contact forms, your message is delivered to our team by email. We do not sell your data. You can delete your account and all saved data at any time from Account &rarr; Security.</p>' +
            '<p>Questions? Email <a href="mailto:rashlast395@gmail.com">rashlast395@gmail.com</a>.</p>'],
        terms: ['Terms of Service', '<p>By placing an order you agree to provide accurate contact and delivery details. Prices are shown in USD and may change without notice. Delivery is free over $30; otherwise a $4.99 fee applies.</p>' +
            '<p>We may refuse or cancel an order if an item is unavailable, an address is outside our delivery area, or we suspect misuse. Promo codes cannot be combined and may expire.</p>' +
            '<p>Opening hours: Wed-Thu 9 AM-10 PM, Fri 9 AM-11 PM, Sat 10 AM-11:30 PM, Sun 11 AM-9 PM. Closed Mon-Tue.</p>'],
        cookies: ['Cookie Policy', '<p>This site stores small pieces of data in your browser (local storage) to keep your cart, wishlist, sign-in session and preferences. We do not use advertising cookies.</p>' +
            '<p>You can clear this data anytime in your browser settings; doing so will sign you out and empty your cart.</p>'],
        shipping: ['Delivery & Returns', '<p><strong>Delivery:</strong> usually 25-35 minutes within our delivery area. Free delivery over $30, otherwise $4.99. You can schedule an order for later at checkout.</p>' +
            '<p><strong>Problems with your order?</strong> Because food is perishable we cannot accept returns, but if something is wrong, missing or not as described, call <a href="tel:0531629095">0531629095</a> within 2 hours and we will remake it or refund you.</p>' +
            '<p><strong>Cancellations:</strong> you can cancel from Account &rarr; Orders while the order is still "Order Received".</p>'],
        faq: ['Frequently Asked Questions', '<details open><summary>How do I order?</summary><p>Tap + on a dish, pick size, spice and extras, add to cart, then check out. You can order as a guest or with an account.</p></details>' +
            '<details><summary>Which payment methods do you accept?</summary><p>Mobile money, bank transfer, card and cash on delivery.</p></details>' +
            '<details><summary>Can I track my order?</summary><p>Yes. A tracking window opens after checkout and all orders are listed under Account &rarr; Orders.</p></details>' +
            '<details><summary>I forgot my password.</summary><p>Use "Forgot password?" on the sign-in form.</p></details>' +
            '<details><summary>Can I change or cancel an order?</summary><p>You can cancel while it is still "Order Received". After that, please call us.</p></details>' +
            '<details><summary>Do you cater for allergies?</summary><p>Add a note at checkout and call us on 0531629095 so the kitchen can advise.</p></details>']
    };
    var legalModal = makeModal('rzLegal', '<div class="rzhead small"><h3 id="lgTitle"></h3></div><div class="rzpane rzlegal" id="lgBody"></div>', 'rzlegalbox');
    function showLegal(k) {
        var d = LEGAL[k]; if (!d) return;
        q('#lgTitle').textContent = d[0]; q('#lgBody').innerHTML = d[1];
        openModal(legalModal);
    }
    document.addEventListener('click', function (e) {
        var a = e.target.closest('[data-legal]');
        if (a) { e.preventDefault(); showLegal(a.getAttribute('data-legal')); }
    });
    qa('.fbot a').forEach(function (a) {
        var t = a.textContent.trim().toLowerCase();
        if (t.indexOf('privacy') > -1) a.setAttribute('data-legal', 'privacy');
        else if (t.indexOf('terms') > -1) a.setAttribute('data-legal', 'terms');
        else if (t.indexOf('cookies') > -1) a.setAttribute('data-legal', 'cookies');
    });
    var quick = q('.flinks');
    if (quick) {
        [['faq', 'fa-circle-question', 'FAQ'], ['shipping', 'fa-truck', 'Delivery &amp; Returns']].forEach(function (x) {
            quick.appendChild(el('<li><a href="#" data-legal="' + x[0] + '"><i class="fas ' + x[1] + '"></i>' + x[2] + '</a></li>'));
        });
        var li = el('<li><a href="#" id="footerAcct"><i class="fas fa-user"></i>My Account</a></li>');
        quick.appendChild(li);
        q('#footerAcct').addEventListener('click', function (e) { e.preventDefault(); acctBtn.click(); });
    }
    /* footer "My Orders" opens the account orders tab */
    var fo = document.getElementById('footerOrders');
    if (fo) {
        var fo2 = fo.cloneNode(true); fo.parentNode.replaceChild(fo2, fo);
        fo2.addEventListener('click', function (e) { e.preventDefault(); openAccount('orders'); });
    }
    var co = document.getElementById('csOrders');
    if (co) {
        var co2 = co.cloneNode(true); co.parentNode.replaceChild(co2, co);
        co2.addEventListener('click', function () { cartBtn.click(); openAccount('orders'); });
    }
    var tmBtn = document.getElementById('tmTrackBtn');
    if (tmBtn) {
        var tm2 = tmBtn.cloneNode(true); tmBtn.parentNode.replaceChild(tm2, tmBtn);
        tm2.addEventListener('click', function () {
            q('#trackModal').classList.remove('open'); q('#tmCvr').classList.remove('show');
            openAccount('orders');
        });
    }

    /* ============================================================
       COOKIE NOTICE
       ============================================================ */
    if (!store.get('rz_cookie', null)) {
        var cb = el('<div class="rzcookie" role="region" aria-label="Cookie notice"><p><i class="fas fa-cookie-bite"></i> We use local storage to keep your cart, wishlist and sign-in. See our <a href="#" data-legal="cookies">Cookie Policy</a>.</p>' +
            '<div><button id="ckOk">Got it</button></div></div>');
        document.body.appendChild(cb);
        setTimeout(function () { cb.classList.add('show'); }, 1200);
        q('#ckOk', cb).addEventListener('click', function () { store.set('rz_cookie', 'ok'); cb.classList.remove('show'); setTimeout(function () { cb.remove(); }, 400); });
    }

    /* ---------- init ---------- */
    refreshAuthUI();
    updateCartUI();
})();
