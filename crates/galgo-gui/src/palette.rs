//! Búsqueda difusa de la paleta de comandos. Lógica pura: sin GUI ni daemon.

/// Puntuación de `query` sobre `text`, o `None` si las letras de la consulta no aparecen en orden. Mayor es mejor:
/// premia las letras seguidas y las que empiezan palabra, y penaliza empezar tarde. Sin distinguir mayúsculas.
pub fn score(query: &str, text: &str) -> Option<i32> {
    let q: Vec<char> = query.to_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
    if q.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut score, mut qi, mut prev, mut first) = (0i32, 0usize, None::<usize>, None::<usize>);
    for (ti, &c) in t.iter().enumerate() {
        if qi < q.len() && c == q[qi] {
            let mut bonus = 1;
            if prev.is_some_and(|p| p + 1 == ti) {
                bonus += 4;
            }
            if ti == 0 || !t[ti - 1].is_alphanumeric() {
                bonus += 3;
            }
            score += bonus;
            first.get_or_insert(ti);
            prev = Some(ti);
            qi += 1;
        }
    }
    if qi < q.len() {
        return None;
    }
    Some(score - first.unwrap_or(0) as i32)
}

/// Índices de `texts` que casan con `query`, del mejor al peor; a igual puntuación, el orden original.
pub fn rank(texts: &[String], query: &str) -> Vec<usize> {
    let mut hits: Vec<(i32, usize)> = texts.iter().enumerate().filter_map(|(i, t)| score(query, t).map(|s| (s, i))).collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    hits.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letras_en_orden_y_sin_mayusculas() {
        assert!(score("cla", "Panel · claude").is_some());
        assert!(score("ALA", "claude").is_none(), "las letras van en orden: a-l-a no aparece");
        assert!(score("zz", "claude").is_none());
        assert_eq!(score("", "cualquier cosa"), Some(0));
    }

    /// Las letras seguidas y al comienzo de palabra puntúan más que las dispersas.
    #[test]
    fn seguidas_y_al_inicio_de_palabra_ganan() {
        let seguida = score("gal", "galgo").unwrap();
        let dispersa = score("gal", "g-a-l").unwrap();
        assert!(seguida > dispersa, "{seguida} > {dispersa}");
        let inicio = score("ws", "workspace").unwrap();
        let medio = score("ws", "showsz").unwrap();
        assert!(inicio > medio, "{inicio} > {medio}");
    }

    #[test]
    fn ranking_del_mejor_al_peor_con_consulta_vacia_el_orden_original() {
        let textos: Vec<String> = ["otro panel", "claude", "claude-2 ocupado"].iter().map(|s| s.to_string()).collect();
        assert_eq!(rank(&textos, "claude"), vec![1, 2]);
        assert_eq!(rank(&textos, ""), vec![0, 1, 2]);
        assert!(rank(&textos, "xq").is_empty());
    }
}
