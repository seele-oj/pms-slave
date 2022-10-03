use async_std::net::TcpStream;
use async_std::prelude::*;
use async_std::task::spawn;

use bincode::Options;

use async_std::task::sleep;
use judge_protocol::handshake::*;
use judge_protocol::judge::*;
use judge_protocol::packet::*;
use judge_protocol::security::*;
use k256::ecdh::EphemeralSecret;
use k256::ecdh::SharedSecret;
use k256::PublicKey;
use rand::thread_rng;
use tempfile::NamedTempFile;

use async_std::channel::{unbounded, Receiver, Sender};
use async_std::sync::*;

use std::fs::File;
use std::io::prelude::*;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use crate::constants::*;
use crate::container::*;
use crate::judge::*;
use crate::language::CompileResult;
use crate::{CONFIG, LANGUAGES, MASTER_PASS};
use log::{debug, error, info};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
enum Actions {
    Reconnect(u64),
    Shutdown,
    Unknown,
}

struct State {
    key: Arc<EphemeralSecret>,
    locked: Mutex<bool>,
    node_id: Mutex<u32>,
    shared: Arc<Mutex<Option<SharedSecret>>>,
    signal: Arc<Mutex<Sender<Actions>>>,
    judge: Arc<Mutex<Option<OnJudge>>>,
}

impl State {
    async fn verify_token(&mut self, mut stream: &mut TcpStream) -> async_std::io::Result<()> {
        let body = BodyAfterHandshake::<()> {
            node_id: (*self.node_id.lock().await),
            client_pubkey: self.key.public_key(),
            req: (),
        };
        let packet = Packet::make_packet(Command::VerifyToken, body.bytes());
        packet.send(Pin::new(&mut stream)).await
    }

    async fn update_judge(
        &self,
        mut stream: &mut TcpStream,
        uuid: Uuid,
        state: JudgeState,
    ) -> async_std::io::Result<()> {
        let body = BodyAfterHandshake {
            node_id: *self.node_id.lock().await,
            client_pubkey: self.key.public_key(),
            req: JudgeResponseBody {
                uuid,
                result: state,
            },
        };
        let packet = Packet::make_packet(
            Command::GetJudgeStateUpdate,
            bincode::DefaultOptions::new()
                .with_big_endian()
                .with_fixint_encoding()
                .serialize(&body)
                .unwrap(),
        );
        packet.send(Pin::new(&mut stream)).await
    }

    async fn handle_command(&mut self, stream: &mut TcpStream, packet: Packet) {
        match packet.heady.header.command {
            Command::Handshake => {
                if let Ok(res) = bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .deserialize::<HandshakeResponse>(&packet.heady.body)
                {
                    match res.result {
                        HandshakeResult::Success => {
                            self.node_id = Mutex::new(res.node_id.unwrap());
                            let shared_key = self.key.diffie_hellman(&res.server_pubkey.unwrap());

                            self.shared = Arc::new(Mutex::new(Some(
                                self.key.diffie_hellman(&res.server_pubkey.unwrap()),
                            )));
                            info!(
                                "Handshake was established from remote {:?}",
                                stream.peer_addr()
                            );
                        }
                        HandshakeResult::PasswordNotMatched => {
                            error!("Master password is not matched. Trying to shutdown ...");
                            self.signal.lock().await.send(Actions::Shutdown).await;
                        }
                        _ => {
                            error!("Unknown detected");
                        }
                    }
                } else {
                    error!("An error occurred on processing Command::Handshake. Trying to reconnect in {} secs ...", SLEEP_TIME);
                    self.signal
                        .lock()
                        .await
                        .send(Actions::Reconnect(SLEEP_TIME))
                        .await;
                }
            }
            Command::ReqVerifyToken => {
                if let Ok(state) = bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .deserialize::<bool>(&packet.heady.body)
                {
                    if !state {
                        info!("Session was expired. Trying to reconnect ...");
                        self.signal.lock().await.send(Actions::Reconnect(0)).await;
                    } else {
                        info!("Command::VerifyToken was succeed");
                    }
                } else {
                    error!("An error occurred on processing Command::ReqVerifyToken. Trying to reconnect in {} secs ...", SLEEP_TIME);
                    self.signal
                        .lock()
                        .await
                        .send(Actions::Reconnect(SLEEP_TIME))
                        .await;
                }
            }
            Command::TestCaseUpdate => {
                if let Ok(test) = bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .deserialize::<TestCaseUpdateBody>(&packet.heady.body)
                {
                    if let Some(onjudge) = self.judge.lock().await.as_ref() {
                        if onjudge.uuid == test.uuid {
                            if *self.locked.lock().await {
                                if let Some(shared_key) = self.shared.lock().await.as_ref() {
                                    let key = expand_key(shared_key);
                                    let (stdin, stdout_origin) =
                                        (test.stdin.decrypt(&key), test.stdout.decrypt(&key));
                                    let (mut stdin_f, mut stdout_origin_f, mut stdout_f) = (
                                        NamedTempFile::new().unwrap(),
                                        NamedTempFile::new().unwrap(),
                                        NamedTempFile::new().unwrap(),
                                    );
                                    stdin_f.write_all(&stdin).ok();
                                    stdout_origin_f.write_all(&stdout_origin).ok();
                                    let (stdin_p, stdout_origin_p, stdout_p) = (
                                        stdin_f.into_temp_path().to_path_buf(),
                                        stdout_origin_f.into_temp_path().to_path_buf(),
                                        stdout_f.into_temp_path().to_path_buf(),
                                    );
                                    let dir = tempfile::tempdir().unwrap();
                                    std::fs::copy(
                                        onjudge.main_binary.clone(),
                                        dir.path().join(BINARY_NAME),
                                    );
                                    let run = Run {
                                        stdin_path: stdin_p.clone(),
                                        stdout_path: stdout_p.clone(),
                                        box_dir: dir.path().to_path_buf(),
                                        language: onjudge.main_lang.clone(),
                                        time_limit: onjudge.time_limit,
                                        mem_limit: onjudge.mem_limit,
                                    };
                                    let res = run.run();
                                    if let Some(status) = res.meta.status {
                                        // Failed?
                                        match status {
                                            _ => {}
                                        }
                                        // Stop judge
                                        *self.locked.lock().await = false;
                                        *self.judge.lock().await = None;
                                    } else {
                                        // Success
                                        // Let's check stdout by checker
                                        let dir_checker = tempfile::tempdir().unwrap();
                                        std::fs::copy(
                                            onjudge.checker_binary.clone(),
                                            dir_checker.path().join(CHECKER_NAME),
                                        );
                                        std::fs::copy(
                                            stdin_p,
                                            dir_checker.path().join(STDIN_FILE_NAME),
                                        );
                                        std::fs::copy(
                                            stdout_p,
                                            dir_checker.path().join(STDOUT_FILE_NAME),
                                        );
                                        std::fs::copy(
                                            stdout_origin_p,
                                            dir_checker.path().join(STDOUT_ORIGIN_FILE_NAME),
                                        );
                                        let checker = CheckerRun {
                                            checker_lang: onjudge.checker_lang.clone(),
                                            box_dir: dir_checker.path().to_path_buf(),
                                        };
                                        let res_checker = checker.run();
                                        if let Some(status_checker) = res_checker.meta.status {
                                            self.update_judge(
                                                stream,
                                                test.uuid,
                                                JudgeState::WrongAnswer(
                                                    test.test_uuid,
                                                    res_checker.meta.time.unwrap() as f64,
                                                    res_checker.meta.cg_mem.unwrap() as f64,
                                                ),
                                            )
                                            .await
                                            .ok();
                                        } else {
                                            self.update_judge(
                                                stream,
                                                test.uuid,
                                                JudgeState::Accepted(
                                                    test.test_uuid,
                                                    res_checker.meta.time.unwrap() as f64,
                                                    res_checker.meta.cg_mem.unwrap() as f64,
                                                ),
                                            )
                                            .await
                                            .ok();
                                        }
                                    }
                                }
                            } else {
                                error!("Unable to handle Command::TestCaseUpdate (JudgeState::UnlockedSlave)");
                                self.update_judge(stream, test.uuid, JudgeState::UnlockedSlave)
                                    .await
                                    .ok();
                            }
                        } else {
                            error!("Unable to handle Command::TestCaseUpdate (JudgeState::JudgeNotFound");
                            self.update_judge(stream, test.uuid, JudgeState::JudgeNotFound)
                                .await
                                .ok();
                        }
                    } else {
                        error!(
                            "Unable to handle Command::TestCaseUpdate (JudgeState::JudgeNotFound"
                        );
                        self.update_judge(stream, test.uuid, JudgeState::JudgeNotFound)
                            .await
                            .ok();
                    }
                }
            }
            Command::GetJudge => {
                if let Ok(judge_req) = bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .deserialize::<JudgeRequestBody>(&packet.heady.body)
                {
                    if !(*self.locked.lock().await) {
                        if let Some(checker_lang) = LANGUAGES.get(judge_req.checker_lang.clone()) {
                            if let Some(main_lang) = LANGUAGES.get(judge_req.main_lang.clone()) {
                                if let Some(shared_key) = self.shared.lock().await.as_ref() {
                                    let key = expand_key(shared_key);
                                    let checker_code = judge_req.checker_code.decrypt(&key);
                                    let main_code = judge_req.main_code.decrypt(&key);
                                    let mut checker_f = NamedTempFile::new().unwrap();
                                    let mut main_f = NamedTempFile::new().unwrap();
                                    *self.locked.lock().await = true;
                                    self.update_judge(
                                        stream,
                                        judge_req.uuid,
                                        JudgeState::DoCompile,
                                    )
                                    .await
                                    .ok();
                                    let c_path = checker_f.into_temp_path().to_path_buf();
                                    let m_path = main_f.into_temp_path().to_path_buf();
                                    let c_res = checker_lang.compile(checker_code, c_path.clone());
                                    let m_res = main_lang.compile(main_code, m_path.clone());
                                    if let CompileResult::Error(stderr) = c_res {
                                        debug!("Unable to compile checker code: {}", stderr);
                                        self.update_judge(
                                            stream,
                                            judge_req.uuid,
                                            JudgeState::InternalError(stderr),
                                        )
                                        .await
                                        .ok();
                                        *self.locked.lock().await = false;
                                    } else {
                                        if let CompileResult::Error(stderr) = m_res {
                                            debug!("Unable to compile main code: {}", stderr);
                                            self.update_judge(
                                                stream,
                                                judge_req.uuid,
                                                JudgeState::CompileError(stderr),
                                            )
                                            .await
                                            .ok();
                                            *self.locked.lock().await = false;
                                        } else {
                                            if let CompileResult::Success(stdout) = m_res {
                                                *self.judge.lock().await = Some(OnJudge {
                                                    uuid: judge_req.uuid,
                                                    main_lang: main_lang.clone(),
                                                    checker_lang: checker_lang.clone(),
                                                    main_binary: m_path,
                                                    checker_binary: c_path,
                                                    time_limit: judge_req.time_limit,
                                                    mem_limit: judge_req.mem_limit,
                                                });
                                                self.update_judge(
                                                    stream,
                                                    judge_req.uuid,
                                                    JudgeState::CompleteCompile(stdout),
                                                )
                                                .await
                                                .ok();
                                            }
                                        }
                                    }
                                } else {
                                    error!("Command::Handshake must be satisfied first");
                                    self.update_judge(
                                        stream,
                                        judge_req.uuid,
                                        JudgeState::InternalError(String::new()),
                                    )
                                    .await
                                    .ok();
                                }
                            } else {
                                error!(
                                    "Unable to get main code language {}",
                                    judge_req.main_lang.clone()
                                );
                            }
                            self.update_judge(stream, judge_req.uuid, JudgeState::LanguageNotFound)
                                .await
                                .ok();
                        } else {
                            error!(
                                "Unable to get checker code language {}",
                                judge_req.checker_lang.clone()
                            );
                            self.update_judge(stream, judge_req.uuid, JudgeState::LanguageNotFound)
                                .await
                                .ok();
                        }
                    } else {
                        self.update_judge(stream, judge_req.uuid, JudgeState::LockedSlave)
                            .await
                            .ok();
                    }
                }
            }
            _ => {
                error!("An unknown command has received");
                // Unknown
            }
        }
    }
}

pub async fn open_protocol() {
    loop {
        let mut shutdown = false;
        // do master connection loop
        if let Ok(_stream) = TcpStream::connect(CONFIG.master).await {
            let stream: Arc<Mutex<TcpStream>> = Arc::new(Mutex::new(_stream));
            let key = EphemeralSecret::random(thread_rng());
            let (send, recv): (Sender<Actions>, Receiver<Actions>) = unbounded();
            let state = Arc::new(Mutex::new(State {
                key: Arc::new(key),
                locked: Mutex::new(false),
                node_id: Mutex::new(std::u32::MAX),
                shared: Arc::new(Mutex::new(None)),
                signal: Arc::new(Mutex::new(send)),
                judge: Arc::new(Mutex::new(None)),
            }));
            let handshake_req = HandshakeRequest {
                client_pubkey: state.lock().await.key.public_key(),
                pass: MASTER_PASS.clone(),
            };
            // Send Handshake packet
            let handshake = Packet::make_packet(
                Command::Handshake,
                bincode::DefaultOptions::new()
                    .with_big_endian()
                    .with_fixint_encoding()
                    .serialize(&handshake_req)
                    .unwrap(),
            );
            handshake
                .send(Pin::new(stream.lock().await.by_ref()))
                .await
                .ok();
            loop {
                if let Ok(actions) = recv.try_recv() {
                    match actions {
                        Actions::Reconnect(secs) => {
                            sleep(Duration::from_secs(secs)).await;
                            break;
                        }
                        Actions::Shutdown => {
                            shutdown = true;
                            break;
                        }
                        _ => {}
                    }
                }
                // TODO: check connection
                // packet loop
                if let Ok(packet) =
                    Packet::from_stream(Pin::new(stream.lock().await.by_ref())).await
                {
                    let state_mutex = Arc::clone(&state);
                    let stream_mutex = Arc::clone(&stream);
                    spawn(async move {
                        state_mutex
                            .lock()
                            .await
                            .handle_command(stream_mutex.lock().await.by_ref(), packet)
                            .await
                    });
                }
            }
            drop(state);
            drop(recv);
        } else {
            error!(
                "Cannot connect to server. Trying to connect in {} secs ...",
                SLEEP_TIME
            );
            sleep(Duration::from_secs(SLEEP_TIME)).await;
        }
        if shutdown {
            info!("Actions::Shutdown was triggered");
            break;
        }
    }
}
