# Pendências do fork

Defeitos que já existiam antes dos PRs #35 e #36 e que os revisores desses PRs encontraram em 11 e 12/11/2026. Ficam registrados aqui porque o fork não tem issues. Cada item também tem um ticket de feedback no ValorBrain. Quando um item for corrigido, o PR do conserto apaga a seção dele.

## 1. Deadlock entre `compact()` manual e o merge do trabalhador

Achado na revisão do #36.

**Reprodução** (sonda `stress.sh` do revisor, PG 18.6 com `shared_preload_libraries = pg_ripple`):
- configuração: `pg_ripple.merge_threshold = 10`, `pg_ripple.merge_interval_secs = 1`, `pg_ripple.worker_database` apontando para o banco do teste e `deadlock_timeout = 1s`;
- dados: 3 predicados com 300 triplas cada, promovidos (`SET pg_ripple.vp_promotion_threshold = 100`, `SELECT pg_ripple.promote_rare_predicates()`);
- carga: por 45 s, ao mesmo tempo, `SELECT pg_ripple.compact()` a cada 1,3 s e um escritor com `pg_ripple.insert_triple(...)` a cada 20 ms.

**Resultado:** de 1 a 3 deadlocks por rodada de 45 s:
- no 46ca4b83, 1 na `compact()`;
- no 725d62c2, 2 na `compact()` e 1 no trabalhador;
- numa rodada de 30 s, o trabalhador ainda teve 1 lock timeout.

```
ERROR:  deadlock detected
DETAIL:  Process 1578464 waits for AccessShareLock on relation 18221 of database 5; blocked by process 1576725.
        Process 1576725 waits for AccessShareLock on relation 18311 of database 5; blocked by process 1578464.
LOG:  pg_ripple merge worker: merge cycle panicked (1): [16908292] deadlock detected
```

No fim não sobra divergência: os padrões conferem com as tabelas VP, e o trabalhador volta no ciclo seguinte. Quem sai perdendo é a `compact()` do usuário, que termina em ERROR.

**Hipótese (não confirmada):** duas fusões rodam ao mesmo tempo, a da `compact()` e a do trabalhador. Cada uma segura uma trava forte nas tabelas de um predicado e espera AccessShare nas de outro: na troca do main, ou na leitura de todas as VP feita pelo rebuild dos padrões. O conserto provável é serializar a fusão da `compact()` e o ciclo do trabalhador com a mesma trava consultiva, ou travar as VP sempre na mesma ordem.

## 2. Segfault intermitente em `sparql_federation` (`register_endpoint`)

Achado na revisão do #35.

Numa rodada da suíte completa de pg_regress na main em que o #35 se baseava (418ba29d), o servidor caiu com segfault no teste `sparql_federation`, nesta chamada:

```sql
SELECT pg_ripple.register_endpoint('http://sparql.local/view-endpoint', 'test_stream');
```

O ambiente era o PG 18.6 do pgrx, com build de debug e sem preload. Os 41 testes que vieram depois da queda foram repetidos e deram saída idêntica. O segfault não voltou em 3 rodadas do PR nem em mais 1 da main: é intermitente. Não há core dump.

**Para reproduzir:** repetir `sparql_federation`, junto com os testes que vêm antes dele na ordem da suíte, em laço e com `ulimit -c unlimited` no postmaster, até capturar o core. Outra opção é rodar sob valgrind ou ASan. Por enquanto, a chamada acima é o único ponto conhecido.

## 3. `maybe_vacuum_dictionary` roda VACUUM por SPI

Achado na revisão do #36.

`maybe_vacuum_dictionary` (`src/dictionary/mod.rs:522`) roda `VACUUM ANALYZE _pg_ripple.dictionary` por `Spi::run` quando uma carga cria `pg_ripple.dict_vacuum_threshold` termos novos ou mais. Só que VACUUM não roda dentro de transação nem de função, então o PostgreSQL levanta ERROR. O `if let Err(e)` em volta só pega erro do SPI; o ERROR do PostgreSQL aborta a carga inteira.

**Reprodução:** o revisor reproduziu com `pg_ripple.dict_vacuum_threshold = 10000`, que é o valor da produção. Basta uma chamada que crie 10 mil termos novos, por exemplo:

```sql
SELECT pg_ripple.load_ntriples(string_agg(
  format('<https://v.test/s%s> <https://v.test/p> <https://v.test/o%s> .', i, i), E'\n'))
FROM generate_series(1, 5000) AS i;
```

A carga termina em ERROR e nada é gravado.

**Contorno:** `pg_ripple.dict_vacuum_threshold = 0`.

**Conserto provável:** trocar o VACUUM por `ANALYZE`, que roda dentro da transação, e deixar a limpeza para o autovacuum ou para um trabalhador que rode fora de transação.

## 4. Instalação nova: o apply worker cai com FATAL por falta de `_pg_ripple.cdc_lsn_watermark`

Achado na revisão do #36.

A tabela `_pg_ripple.cdc_lsn_watermark` só é criada em dois lugares:
- no script de atualização `sql/pg_ripple--0.80.0--0.81.0.sql`;
- em `cdc::initialize_cdc_schema()`, que só é chamada por `storage::initialize_schema()`, e essa função não é chamada por ninguém hoje (`#[allow(dead_code)]`).

O SQL de instalação da 0.141.0 não cria a tabela. Um banco que passou pela 0.81, como o da produção, tem a tabela; um `CREATE EXTENSION` novo não tem.

Com `pg_ripple.replication_enabled = on`, o apply worker lógico (`src/replication.rs:208-216`) grava o watermark a cada lote, por `cdc::update_lsn_watermark`. Sem a tabela, o `INSERT` falha com "relation does not exist", e o ERROR dentro de `BackgroundWorker::transaction` derruba o trabalhador com FATAL.

**Reprodução:**
1. Cluster novo com `shared_preload_libraries = pg_ripple`, `pg_ripple.replication_enabled = on` e `pg_ripple.worker_database` apontando para o banco.
2. `CREATE EXTENSION pg_ripple`.
3. Inserir linhas em `_pg_ripple.replication_status`.
4. O log mostra o FATAL do apply worker.

A sonda do revisor contorna o problema criando a tabela à mão:

```sql
CREATE TABLE IF NOT EXISTS _pg_ripple.cdc_lsn_watermark (
  slot_name TEXT PRIMARY KEY, last_lsn PG_LSN NOT NULL, updated_at TIMESTAMPTZ NOT NULL DEFAULT now());
```

**Conserto provável:** criar a tabela no SQL de instalação (`extension_sql!`) e também numa migração idempotente, para quem instalou direto numa versão que não tem a tabela.

## 5. Tombstone de predicado que só teve delete nunca é aplicado; `drop_graph` repetido dobra tombstones

Visto ao escrever o `v0147h` (#37).

- `merge_all` (`src/storage/merge.rs:802`) só faz merge de predicado com linha no delta. Um predicado que só recebeu deletes fica com os tombstones e com as linhas mortas no main até a próxima inserção; a view continua na forma com `LEFT JOIN` nos tombstones.
- Um segundo `drop_graph` do mesmo grafo grava tombstone de novo para as linhas já marcadas: no teste, 930 tombstones para 465 linhas.

**Reprodução:** carregar triplas num grafo, rodar `compact()`, depois `drop_graph` duas vezes e `compact()`. O main continua com as linhas, `vp_{id}_tombstones` fica com o dobro e o `compact()` não as aplica.

## 6. `construct_rules/retract.rs:77` grava tombstone sem reconstruir a view

Achado na revisão do #37. Não foi reproduzido.

O caminho de retração do CONSTRUCT grava em `vp_{id}_tombstones` sem chamar `rebuild_htap_view`. Se o predicado estava sem tombstones, a view fica na forma sem o `LEFT JOIN` e continua devolvendo a linha apagada. Isso quebra a premissa do anti-join do #37 quando o mesmo predicado é alvo de CONSTRUCT e cabeça de regra Datalog: a passada pula uma linha que deveria rederivar.

## 7. `er_monitoring_prune` usa `.ok()`, mas o erro do PostgreSQL vira panic

Achado na revisão do #36 (P1). A produção tem as tabelas e não é afetada.

Em `src/er_monitoring.rs:139-165`: sem a tabela `_pg_ripple.er_unresolved_entities`, que só `enable_er_monitoring()` cria, o ciclo do trabalhador 0 termina em panic logo depois do merge, em todo ciclo, e o trabalhador fica em backoff permanente. O `.ok()` em volta da chamada não pega o ERROR do PostgreSQL.

## 8. `build.rs` só recalcula o `GIT_SHA` quando o `.versions.toml` muda

Achado na revisão do #36 (P4).

Num build incremental, a linha `loaded (release build, … git <sha>)` do log e o `build_info()` podem trazer o commit de um build anterior (a revisão viu "git 725d62c2" num build da main). O kit da janela contorna com `cargo clean -p pg_ripple --release` e `GIT_SHA=<8>` exportado, e confere o `build_info()`. Conserto provável: `cargo::rerun-if-changed` para `.git/HEAD` e para a ref atual, ou `rerun-if-env-changed=GIT_SHA`.

## 9. O `v0147d` não roda duas vezes no mesmo banco

Achado na revisão do #37.

Na suíte, cada teste roda uma vez num banco novo, então nada quebra hoje. Mas, ao contrário do `v0147g` e do `v0147h`, o `v0147d` não passa numa segunda rodada no mesmo banco, o que atrapalha quem repete o teste para medir. A causa não foi investigada.

## 10. Depois de um restart, delta não vazio só é mergeado quando chega uma escrita nova

Visto na segunda janela de módulos (12/11/2026, main 68011562).

- O ciclo do trabalhador sai logo no começo quando `shmem::delta_is_empty()` dá verdadeiro (`src/worker.rs:540`). Essa função só olha o contador em memória `TOTAL_DELTA_ROWS` (`src/shmem.rs:247-268`), que zera no restart e só sobe em `record_delta_inserts`. Quem chama `record_delta_inserts` é o `insert_triple` e as cargas em lote (`src/storage/ops/mod.rs:130`, `241`, `339`, `510`; `src/storage/ops/dedup.rs:159`).
- A inferência não incrementa o contador. O INSERT do caminho simples (`compile_single_rule`, `src/datalog/compiler/rule.rs:46`) e a materialização do semi-naive (`materialise_derived_rows`, `src/datalog/seminaive.rs:37`) gravam no `vp_{id}_delta` por SQL e não chamam `record_delta_inserts`. Um delta que só cresce por inferência só é mergeado quando chega alguma escrita contada, e cada merge zera o contador de novo (`src/worker.rs:714`, `src/storage/merge.rs:965`).
- **Medição:** depois do restart das 12:27, o contador estava em 0 com 41.259 linhas no `vp_534_delta`. O trabalhador releu o `merge_threshold` no SIGHUP, mas não olhou as tabelas. Às 12:36 o motor gravou 22 triplas, o contador saiu de 0, e o ciclo seguinte fez o merge (41.259 → 0 linhas no delta, fecho igual).
- **Conserto sugerido:** no primeiro ciclo depois de subir e depois de um SIGHUP, avaliar os deltas uma vez sem depender do contador, ou iniciar o contador com uma contagem limitada das tabelas de delta. Além disso, fazer a inferência chamar `record_delta_inserts` com o número de linhas gravadas.

## 11. Roteiro de janela: o modo `limite-padrao` precisa de uma pré-condição sobre o contador

Visto na mesma janela.

- Na primeira tentativa, o `limite-padrao` do roteiro (`/var/backups/janela-modulos-2026-11b/janela.sh`) falhou com "o trabalhador não leu o valor novo". Nesse caso a mensagem está errada: o trabalhador releu o limite (logou o SIGHUP), mas o ciclo saiu cedo porque o contador estava zerado (item 10).
- Para a próxima janela:
  - exigir `pg_ripple.stats()->>'unmerged_delta_rows' > 0` antes do RESET; é o mesmo contador (`src/stats_admin.rs:354`);
  - trocar a mensagem por uma que separe "não releu o limite" de "não havia escrita contada".
