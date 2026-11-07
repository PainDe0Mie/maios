// Ce fichier implémente la protection contre les débordements de pile (Stack Smashing).
// Le compilateur insère automatiquement un "canari" (une valeur secrète) dans la pile
// au début des fonctions vulnérables, et vérifie cette valeur à la fin.

// 1. LE CANARI (__stack_chk_guard)
// C'est la valeur de référence. Dans un OS finalisé, on génère cette valeur 
// aléatoirement au tout début du démarrage (via un générateur de nombres aléatoires matériel).
// Pour l'instant, on met une valeur magique "en dur".
// L'octet `00` à la fin est une astuce classique : si une attaque utilise des fonctions 
// de chaînes de caractères (comme strcpy), elle s'arrêtera au premier `00` et ne pourra pas 
// réécrire la suite du canari correctement.

#[no_mangle]
#[cfg(target_pointer_width = "64")] // Pour ton OS en 64 bits (x86_64 ou AArch64)
pub static __stack_chk_guard: usize = 0x595E9ABEBEEF0000;

#[no_mangle]
#[cfg(target_pointer_width = "32")] // Au cas où tu compiles en 32 bits plus tard
pub static __stack_chk_guard: usize = 0xE2000000;


// 2. LA FONCTION D'ERREUR (__stack_chk_fail)
// Si le compilateur détecte que la valeur sur la pile ne correspond plus à `__stack_chk_guard`,
// il saute immédiatement dans cette fonction au lieu de retourner de la fonction en cours.
// On utilise #[no_mangle] pour que le compilateur trouve exactement ce nom.

#[no_mangle]
pub extern "C" fn __stack_chk_fail() -> ! {
    // Si on arrive ici, un buffer a débordé sur la pile et a écrasé notre canari.
    // L'adresse de retour de la fonction a peut-être été piratée ou corrompue.
    // On crash l'OS intentionnellement pour éviter l'exécution de code malveillant ou des bugs pire.
    panic!("ALERTE SÉCURITÉ : __stack_chk_fail appelé ! Stack smashing (corruption de la pile) détecté !");
}
