//! Table des objets d'une connexion.
//!
//! Deux plages, comme libwayland : les identifiants du client (`1 ..= 0xfeffffff`) et ceux du
//! serveur (`0xff000000 ..`). Les identifiants du client sont **denses** : un nouvel objet prend
//! une place libérée ou exactement la suivante (`wl_map_insert_at`). Un client ne peut donc pas
//! faire grossir la table d'un coup avec un identifiant immense, et la table est bornée
//! (`limit`, cf. CVE-2021-3782 : un client qui crée des objets sans fin).
//!
//! Un objet du client détruit par le serveur devient un **zombie** jusqu'à ce que le client
//! réutilise son identifiant : les requêtes déjà en route vers lui sont décodées (pour consommer
//! leurs descripteurs et garder le flux aligné) puis ignorées.

use alloc::vec::Vec;

use crate::types::{Interface, ObjectId, DISPLAY_ID, SERVER_ID_START};

/// Objet vivant.
#[derive(Debug)]
pub struct Object<D> {
    pub interface: &'static Interface,
    pub version: u32,
    pub data: D,
}

#[derive(Debug)]
enum Slot<D> {
    Free,
    Live(Object<D>),
    Zombie(&'static Interface),
}

/// Résultat d'une recherche.
#[derive(Debug)]
pub enum Lookup<'a, D> {
    Live(&'a Object<D>),
    /// Détruit par le serveur, identifiant pas encore réutilisé par le client.
    Zombie(&'static Interface),
    Unknown,
}

/// Refus d'un nouvel identifiant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IdError {
    /// Hors de la plage du client, ou au-delà de la place suivante.
    OutOfRange,
    /// Déjà utilisé.
    InUse,
    /// Trop d'objets pour cette connexion.
    TooMany,
}

/// Table des objets d'une connexion, `D` étant la donnée du serveur pour chaque objet.
#[derive(Debug)]
pub struct ObjectMap<D> {
    client: Vec<Slot<D>>,
    server: Vec<Slot<D>>,
    server_free: Vec<u32>,
    live: usize,
    limit: usize,
}

impl<D> ObjectMap<D> {
    /// Table avec `wl_display` (identifiant 1) ; `limit` : places au plus, toutes plages.
    pub fn new(limit: usize, display: D) -> Self {
        let mut m = ObjectMap { client: Vec::new(), server: Vec::new(), server_free: Vec::new(), live: 0, limit: limit.max(1) };
        let iface = &crate::protocols::wl_display::INTERFACE;
        m.client.push(Slot::Live(Object { interface: iface, version: 1, data: display }));
        m.live = 1;
        m
    }

    /// Objets vivants.
    pub fn live(&self) -> usize {
        self.live
    }

    fn slots(&self, id: ObjectId) -> Option<(&Vec<Slot<D>>, usize)> {
        if id == 0 {
            None
        } else if id < SERVER_ID_START {
            Some((&self.client, (id - 1) as usize))
        } else {
            Some((&self.server, (id - SERVER_ID_START) as usize))
        }
    }

    fn slot_mut(&mut self, id: ObjectId) -> Option<&mut Slot<D>> {
        if id == 0 {
            None
        } else if id < SERVER_ID_START {
            self.client.get_mut((id - 1) as usize)
        } else {
            self.server.get_mut((id - SERVER_ID_START) as usize)
        }
    }

    pub fn lookup(&self, id: ObjectId) -> Lookup<'_, D> {
        match self.slots(id).and_then(|(v, i)| v.get(i)) {
            Some(Slot::Live(o)) => Lookup::Live(o),
            Some(Slot::Zombie(i)) => Lookup::Zombie(i),
            _ => Lookup::Unknown,
        }
    }

    pub fn get(&self, id: ObjectId) -> Option<&Object<D>> {
        match self.lookup(id) {
            Lookup::Live(o) => Some(o),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, id: ObjectId) -> Option<&mut Object<D>> {
        match self.slot_mut(id) {
            Some(Slot::Live(o)) => Some(o),
            _ => None,
        }
    }

    /// Le client peut-il créer un objet d'identifiant `id` ?
    pub fn check_client_id(&self, id: ObjectId) -> Result<(), IdError> {
        if id == 0 || id >= SERVER_ID_START {
            return Err(IdError::OutOfRange);
        }
        let i = (id - 1) as usize;
        match self.client.get(i) {
            Some(Slot::Live(_)) => Err(IdError::InUse),
            Some(_) => Ok(()),
            None if i == self.client.len() => {
                if self.client.len() + self.server.len() >= self.limit {
                    Err(IdError::TooMany)
                } else {
                    Ok(())
                }
            }
            None => Err(IdError::OutOfRange),
        }
    }

    /// Crée l'objet `id` demandé par le client.
    pub fn insert_client(&mut self, id: ObjectId, interface: &'static Interface, version: u32, data: D) -> Result<(), IdError> {
        self.check_client_id(id)?;
        let o = Slot::Live(Object { interface, version, data });
        let i = (id - 1) as usize;
        if i == self.client.len() {
            self.client.push(o);
        } else {
            self.client[i] = o;
        }
        self.live += 1;
        Ok(())
    }

    /// Crée un objet du serveur (annoncé au client par un `new_id` d'événement).
    pub fn insert_server(&mut self, interface: &'static Interface, version: u32, data: D) -> Result<ObjectId, IdError> {
        let o = Slot::Live(Object { interface, version, data });
        let i = match self.server_free.pop() {
            Some(i) => {
                self.server[i as usize] = o;
                i
            }
            None => {
                if self.client.len() + self.server.len() >= self.limit
                    || self.server.len() as u64 >= u64::from(u32::MAX - SERVER_ID_START)
                {
                    return Err(IdError::TooMany);
                }
                self.server.push(o);
                (self.server.len() - 1) as u32
            }
        };
        self.live += 1;
        Ok(SERVER_ID_START + i)
    }

    /// Le client a détruit `id` (requête destructrice) : la place est libre aussitôt. Pour un
    /// identifiant du client, le serveur doit ensuite envoyer `wl_display.delete_id`.
    pub fn remove(&mut self, id: ObjectId) -> Option<D> {
        if id == DISPLAY_ID {
            return None;
        }
        let server = id >= SERVER_ID_START;
        let slot = self.slot_mut(id)?;
        match core::mem::replace(slot, Slot::Free) {
            Slot::Live(o) => {
                self.live -= 1;
                if server {
                    self.server_free.push(id - SERVER_ID_START);
                }
                Some(o.data)
            }
            other => {
                *slot = other;
                None
            }
        }
    }

    /// Le serveur détruit `id` de lui-même : un objet du client devient zombie (ses requêtes en
    /// route sont ignorées), un objet du serveur est libéré.
    pub fn kill(&mut self, id: ObjectId) -> Option<D> {
        if id == DISPLAY_ID {
            return None;
        }
        if id >= SERVER_ID_START {
            return self.remove(id);
        }
        let slot = self.slot_mut(id)?;
        let Slot::Live(o) = core::mem::replace(slot, Slot::Free) else {
            return None;
        };
        *slot = Slot::Zombie(o.interface);
        self.live -= 1;
        Some(o.data)
    }

    /// Objets vivants, dans l'ordre des identifiants.
    pub fn iter(&self) -> impl Iterator<Item = (ObjectId, &Object<D>)> + '_ {
        let c = self.client.iter().enumerate().map(|(i, s)| (i as u32 + 1, s));
        let s = self.server.iter().enumerate().map(|(i, s)| (SERVER_ID_START + i as u32, s));
        c.chain(s).filter_map(|(id, s)| match s {
            Slot::Live(o) => Some((id, o)),
            _ => None,
        })
    }

    /// Vide la table (déconnexion) : rend les données de tous les objets vivants.
    pub fn drain(&mut self) -> Vec<(ObjectId, Object<D>)> {
        let mut out = Vec::with_capacity(self.live);
        for (i, s) in core::mem::take(&mut self.client).into_iter().enumerate() {
            if let Slot::Live(o) = s {
                out.push((i as u32 + 1, o));
            }
        }
        for (i, s) in core::mem::take(&mut self.server).into_iter().enumerate() {
            if let Slot::Live(o) = s {
                out.push((SERVER_ID_START + i as u32, o));
            }
        }
        self.server_free.clear();
        self.live = 0;
        out
    }
}
