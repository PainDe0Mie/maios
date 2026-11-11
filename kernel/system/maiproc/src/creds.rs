//! Identités d'un processus (`struct cred` de Linux) : uid et gid réels, effectifs, sauvegardés
//! et de système de fichiers, groupes supplémentaires, capacités et espace utilisateur.
//!
//! Les identités sont **globales** (`kuid_t`) : les appels système traduisent par
//! `user_ns` (voir [`crate::userns`]). Les règles de `setuid(2)`, `setreuid(2)`,
//! `setresuid(2)`, `setfsuid(2)` et de leurs pendants pour les groupes sont celles de Linux :
//! avec `CAP_SETUID`/`CAP_SETGID` dans son espace, un processus prend n'importe quelle identité
//! (projetée) ; sinon il ne peut que permuter entre ses identités réelle, effective et
//! sauvegardée.
//!
//! Capacités (`capabilities(7)`) : ensembles effectif, permis, héritable, limitant et ambiant.
//! Transitions de Linux sans capacités de fichier (pas d'attributs étendus) :
//! - changement d'uid (`cap_emulate_setxuid`) : quitter root (aucun des uid réel, effectif,
//!   sauvegardé ne vaut root de l'espace) vide permis et effectif (sauf `PR_SET_KEEPCAPS`) et
//!   l'ambiant ; l'effectif suit l'uid effectif ; les capacités de fichiers suivent `fsuid` ;
//! - `execve` (`cap_bprm_creds_from_file`) : pour root, permis = limitant | héritable ;
//!   puis permis |= ambiant, effectif = permis si l'uid effectif est root, l'ambiant sinon ;
//!   un programme `set-user-ID`/`set-group-ID` vide l'ambiant.
//!
//! Droits d'accès (`may_access`, `generic_permission`) : contrôle discrétionnaire POSIX, puis
//! `CAP_DAC_OVERRIDE`/`CAP_DAC_READ_SEARCH` si le propriétaire et le groupe du fichier sont
//! projetés dans l'espace (`capable_wrt_inode_uidgid`).
//!
//! Les identités sont celles du processus (les fils les partagent) : la glibc diffuse déjà
//! chaque changement à tous les fils (`SIGSETXID`), qui arrivent ici au même résultat.

use alloc::{sync::Arc, vec::Vec};

use crate::linux::errno::{EINVAL, EPERM};
use crate::userns::{self, bit, UserNs, CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FS_MASK, CAP_FULL, CAP_SETGID, CAP_SETUID};

pub const NGROUPS_MAX: usize = 65536;

/// `SECURE_KEEP_CAPS` (bit 4 de `securebits`, `PR_SET_KEEPCAPS`).
pub const SECURE_KEEP_CAPS: u32 = 4;

#[derive(Clone)]
pub struct Creds {
    pub ruid: u32,
    pub euid: u32,
    pub suid: u32,
    pub fsuid: u32,
    pub rgid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub fsgid: u32,
    pub groups: Vec<u32>,
    pub cap_inh: u64,
    pub cap_prm: u64,
    pub cap_eff: u64,
    pub cap_bnd: u64,
    pub cap_amb: u64,
    pub securebits: u32,
    pub user_ns: Arc<UserNs>,
}

impl core::fmt::Debug for Creds {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Creds(uid {}/{}/{}/{} gid {}/{}/{}/{} eff {:#x} ns {})", self.ruid, self.euid, self.suid, self.fsuid, self.rgid, self.egid, self.sgid, self.fsgid, self.cap_eff, self.user_ns.ino)
    }
}

/// `-1` d'un argument d'identité : inchangé.
pub const KEEP: u32 = u32::MAX;

/// Accès demandés (`R_OK`, `W_OK`, `X_OK`).
pub const MAY_READ: u32 = 4;
pub const MAY_WRITE: u32 = 2;
pub const MAY_EXEC: u32 = 1;

/// Bits de mode qui intéressent `execve` : propriétaire et droits du programme.
#[derive(Clone, Copy)]
pub struct ExecFile {
    pub perm: u16,
    pub uid: u32,
    pub gid: u32,
}

impl Creds {
    /// Identités de root dans l'espace initial, toutes capacités.
    pub fn root() -> Creds {
        Creds {
            ruid: 0,
            euid: 0,
            suid: 0,
            fsuid: 0,
            rgid: 0,
            egid: 0,
            sgid: 0,
            fsgid: 0,
            groups: Vec::new(),
            cap_inh: 0,
            cap_prm: CAP_FULL,
            cap_eff: CAP_FULL,
            cap_bnd: CAP_FULL,
            cap_amb: 0,
            securebits: 0,
            user_ns: userns::init_user_ns(),
        }
    }

    /// root de l'espace des identités, en identité globale (`None` : 0 n'y est pas projeté).
    fn root_uid(&self) -> Option<u32> {
        self.user_ns.make_kuid(0)
    }

    fn has_cap(&self, cap: u32) -> bool {
        self.cap_eff & bit(cap) != 0
    }

    fn in_uids(&self, u: u32) -> bool {
        u == self.ruid || u == self.euid || u == self.suid
    }

    fn in_gids(&self, g: u32) -> bool {
        g == self.rgid || g == self.egid || g == self.sgid
    }

    pub fn in_group(&self, g: u32) -> bool {
        self.fsgid == g || self.groups.contains(&g)
    }

    /// `cap_emulate_setxuid` + `cap_task_fix_setuid` : capacités après un changement d'uid.
    fn fix_setuid(&mut self, old: &Creds) {
        let root = self.root_uid();
        let is_root = |u: u32| Some(u) == root;
        let old_any_root = is_root(old.ruid) || is_root(old.euid) || is_root(old.suid);
        let new_any_root = is_root(self.ruid) || is_root(self.euid) || is_root(self.suid);
        if old_any_root && !new_any_root && self.securebits & (1 << SECURE_KEEP_CAPS) == 0 {
            self.cap_prm = 0;
            self.cap_eff = 0;
        }
        if old_any_root && !new_any_root {
            self.cap_amb = 0;
        }
        if is_root(old.euid) && !is_root(self.euid) {
            self.cap_eff = 0;
        }
        if !is_root(old.euid) && is_root(self.euid) {
            self.cap_eff = self.cap_prm;
        }
        self.fix_fsuid(old.fsuid);
    }

    /// Capacités de système de fichiers selon le passage de `fsuid` à root ou hors de root.
    fn fix_fsuid(&mut self, old_fsuid: u32) {
        let root = self.root_uid();
        let was = Some(old_fsuid) == root;
        let now = Some(self.fsuid) == root;
        if was && !now {
            self.cap_eff &= !CAP_FS_MASK;
        }
        if !was && now {
            self.cap_eff |= self.cap_prm & CAP_FS_MASK;
        }
    }

    pub fn setuid(&mut self, u: u32) -> Result<(), i64> {
        if u == KEEP {
            return Err(EINVAL);
        }
        let old = self.clone();
        if self.has_cap(CAP_SETUID) {
            (self.ruid, self.euid, self.suid, self.fsuid) = (u, u, u, u);
        } else if u == self.ruid || u == self.suid {
            (self.euid, self.fsuid) = (u, u);
        } else {
            return Err(EPERM);
        }
        self.fix_setuid(&old);
        Ok(())
    }

    pub fn setgid(&mut self, g: u32) -> Result<(), i64> {
        if g == KEEP {
            return Err(EINVAL);
        }
        if self.has_cap(CAP_SETGID) {
            (self.rgid, self.egid, self.sgid, self.fsgid) = (g, g, g, g);
        } else if g == self.rgid || g == self.sgid {
            (self.egid, self.fsgid) = (g, g);
        } else {
            return Err(EPERM);
        }
        Ok(())
    }

    pub fn setreuid(&mut self, r: u32, e: u32) -> Result<(), i64> {
        let priv_ = self.has_cap(CAP_SETUID);
        if r != KEEP && !priv_ && r != self.ruid && r != self.euid {
            return Err(EPERM);
        }
        if e != KEEP && !priv_ && !self.in_uids(e) {
            return Err(EPERM);
        }
        let old = self.clone();
        if r != KEEP {
            self.ruid = r;
        }
        if e != KEEP {
            self.euid = e;
        }
        // Comme Linux : l'identité sauvegardée prend l'effective si la réelle change, ou si
        // l'effective devient autre que l'ancienne réelle.
        if r != KEEP || (e != KEEP && e != old.ruid) {
            self.suid = self.euid;
        }
        self.fsuid = self.euid;
        self.fix_setuid(&old);
        Ok(())
    }

    pub fn setregid(&mut self, r: u32, e: u32) -> Result<(), i64> {
        let priv_ = self.has_cap(CAP_SETGID);
        if r != KEEP && !priv_ && r != self.rgid && r != self.egid {
            return Err(EPERM);
        }
        if e != KEEP && !priv_ && !self.in_gids(e) {
            return Err(EPERM);
        }
        let old_rgid = self.rgid;
        if r != KEEP {
            self.rgid = r;
        }
        if e != KEEP {
            self.egid = e;
        }
        if r != KEEP || (e != KEEP && e != old_rgid) {
            self.sgid = self.egid;
        }
        self.fsgid = self.egid;
        Ok(())
    }

    pub fn setresuid(&mut self, r: u32, e: u32, s: u32) -> Result<(), i64> {
        if !self.has_cap(CAP_SETUID) && [r, e, s].iter().any(|&v| v != KEEP && !self.in_uids(v)) {
            return Err(EPERM);
        }
        let old = self.clone();
        if r != KEEP {
            self.ruid = r;
        }
        if e != KEEP {
            self.euid = e;
        }
        if s != KEEP {
            self.suid = s;
        }
        self.fsuid = self.euid;
        self.fix_setuid(&old);
        Ok(())
    }

    pub fn setresgid(&mut self, r: u32, e: u32, s: u32) -> Result<(), i64> {
        if !self.has_cap(CAP_SETGID) && [r, e, s].iter().any(|&v| v != KEEP && !self.in_gids(v)) {
            return Err(EPERM);
        }
        if r != KEEP {
            self.rgid = r;
        }
        if e != KEEP {
            self.egid = e;
        }
        if s != KEEP {
            self.sgid = s;
        }
        self.fsgid = self.egid;
        Ok(())
    }

    /// `setfsuid` : renvoie toujours l'ancien uid de système de fichiers ; ne change que si
    /// permis.
    pub fn setfsuid(&mut self, u: u32) -> u32 {
        let old = self.fsuid;
        if u != KEEP && (self.has_cap(CAP_SETUID) || self.in_uids(u) || u == self.fsuid) {
            self.fsuid = u;
            self.fix_fsuid(old);
        }
        old
    }

    pub fn setfsgid(&mut self, g: u32) -> u32 {
        let old = self.fsgid;
        if g != KEEP && (self.has_cap(CAP_SETGID) || self.in_gids(g) || g == self.fsgid) {
            self.fsgid = g;
        }
        old
    }

    /// `setgroups` : `CAP_SETGID` dans l'espace, et `setgroups` permis par l'espace
    /// (`may_setgroups`).
    pub fn setgroups(&mut self, groups: Vec<u32>) -> Result<(), i64> {
        if !self.has_cap(CAP_SETGID) || !self.user_ns.may_setgroups() {
            return Err(EPERM);
        }
        let mut g = groups;
        g.sort_unstable();
        g.dedup();
        self.groups = g;
        Ok(())
    }

    /// Entrée dans un nouvel espace utilisateur (`set_cred_user_ns`) : toutes les capacités
    /// dans cet espace, aucune héritable ni ambiante.
    pub fn enter_user_ns(&mut self, ns: Arc<UserNs>) {
        self.securebits = 0;
        self.cap_inh = 0;
        self.cap_prm = CAP_FULL;
        self.cap_eff = CAP_FULL;
        self.cap_amb = 0;
        self.cap_bnd = CAP_FULL;
        self.user_ns = ns;
    }

    /// `execve` : bits `set-user-ID` / `set-group-ID` du programme (ignorés si `nosuid` :
    /// montage `nosuid`, `no_new_privs` ou propriétaire sans projection), puis capacités
    /// (`cap_bprm_creds_from_file`) et identités sauvegardées = effectives. Sous `no_new_privs`
    /// (`LSM_UNSAFE_NO_NEW_PRIVS`), un `execve` ne gagne rien : le permis est borné à l'ancien
    /// et les identités effectives ramenées aux réelles — c'est ainsi que root, après un
    /// `capset` qui vide tout (bubblewrap `--cap-drop ALL`), ne retrouve pas ses capacités
    /// par l'ensemble limitant.
    pub fn on_exec(&mut self, file: Option<ExecFile>, nosuid: bool, no_new_privs: bool) {
        const S_ISUID: u16 = 0o4000;
        const S_ISGID: u16 = 0o2000;
        let old = self.clone();
        if let (Some(f), false) = (file, nosuid) {
            if self.user_ns.has_kuid(f.uid) && self.user_ns.has_kgid(f.gid) {
                if f.perm & S_ISUID != 0 {
                    self.euid = f.uid;
                }
                // `S_ISGID` sans bit `x` du groupe : verrouillage obligatoire, pas un changement
                // d'identité.
                if f.perm & S_ISGID != 0 && f.perm & 0o010 != 0 {
                    self.egid = f.gid;
                }
            }
        }
        // Pas de capacités de fichier : le permis repart de zéro.
        self.cap_prm = 0;
        let root = self.root_uid();
        let mut effective = false;
        if root.is_some() && (Some(self.euid) == root || Some(self.ruid) == root) {
            self.cap_prm = old.cap_bnd | old.cap_inh;
        }
        if root.is_some() && Some(self.euid) == root {
            effective = true;
        }
        let is_setid = self.euid != old.euid || self.egid != old.egid;
        // `__cap_gained(permitted)` ou changement d'identité dans un contexte « dangereux » :
        // `no_new_privs` (MaiOS n'a pas de `ptrace`, l'autre cas de `bprm->unsafe`).
        if (is_setid || self.cap_prm & !old.cap_prm != 0) && no_new_privs {
            self.euid = self.ruid;
            self.egid = self.rgid;
            self.cap_prm &= old.cap_prm;
        }
        if is_setid {
            self.cap_amb = 0;
        }
        self.cap_prm |= self.cap_amb;
        self.cap_eff = if effective { self.cap_prm } else { self.cap_amb };
        self.suid = self.euid;
        self.sgid = self.egid;
        self.fsuid = self.euid;
        self.fsgid = self.egid;
        self.securebits &= !(1 << SECURE_KEEP_CAPS);
    }

    /// `capable_wrt_inode_uidgid` : capacité dans l'espace des identités, propriétaire et groupe
    /// du fichier projetés dans cet espace.
    fn capable_wrt(&self, eff: u64, m: &fs_node::Metadata, cap: u32) -> bool {
        eff & bit(cap) != 0 && self.user_ns.has_kuid(m.uid) && self.user_ns.has_kgid(m.gid)
    }

    /// `capable_wrt_inode_uidgid` avec les capacités effectives.
    pub fn inode_capable(&self, m: &fs_node::Metadata, cap: u32) -> bool {
        self.capable_wrt(self.cap_eff, m, cap)
    }

    /// Contrôle d'accès (`generic_permission`) avec les identités de système de fichiers
    /// (`real` : les identités réelles, pour `access(2)` sans `AT_EACCESS` — capacités vidées
    /// si l'uid réel n'est pas root, comme `access_override_creds`).
    pub fn may_access(&self, m: &fs_node::Metadata, want: u32, real: bool) -> bool {
        let (uid, gid) = if real { (self.ruid, self.rgid) } else { (self.fsuid, self.fsgid) };
        let eff = if real {
            if Some(self.ruid) == self.root_uid() { self.cap_prm } else { 0 }
        } else {
            self.cap_eff
        };
        let perm = m.perm as u32;
        let bits = if m.uid == uid {
            (perm >> 6) & 7
        } else if m.gid == gid || self.groups.contains(&m.gid) {
            (perm >> 3) & 7
        } else {
            perm & 7
        };
        if bits & want == want {
            return true;
        }
        if m.kind == fs_node::NodeKind::Dir {
            if want & MAY_WRITE == 0 && self.capable_wrt(eff, m, CAP_DAC_READ_SEARCH) {
                return true;
            }
            return self.capable_wrt(eff, m, CAP_DAC_OVERRIDE);
        }
        if want == MAY_READ && self.capable_wrt(eff, m, CAP_DAC_READ_SEARCH) {
            return true;
        }
        // Exécuter un fichier sans aucun bit `x` reste interdit, même à root.
        (want & MAY_EXEC == 0 || perm & 0o111 != 0) && self.capable_wrt(eff, m, CAP_DAC_OVERRIDE)
    }

    /// Le processus de ces identités peut-il agir sur un processus d'identités `other`
    /// (signaux, priorités) : même uid réel ou effectif, ou `cap` dans l'espace de `other`.
    pub fn may_act_on(&self, other: &Creds, cap: u32) -> bool {
        self.euid == other.suid
            || self.euid == other.ruid
            || self.ruid == other.suid
            || self.ruid == other.ruid
            || userns::ns_capable(self, &other.user_ns, cap)
    }
}
