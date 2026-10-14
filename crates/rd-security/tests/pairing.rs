//! T-013 配对协议测试：正确码成功、错误码失败、过期失败、一次性、5 次失败锁定、
//! 指纹不一致（模拟中间人）失败。
//!
//! 协议步骤：1 控制端 SPAKE2 → 2 Agent SPAKE2 → 3 控制端 MAC → 4 Agent MAC（控制端 Done）。

use std::time::Instant;

use rd_security::{AgentPairing, ControllerPairing, PairError, PairStep};
use rd_transport::Fingerprint;

/// 构造一个确定的测试指纹。
fn fp(seed: u8) -> Fingerprint {
    Fingerprint([seed; 32])
}

/// 与 `code` 必不相同的一个 6 位码。
fn wrong_code(code: &str) -> String {
    let n = code.parse::<u32>().expect("配对码是 6 位数字");
    format!("{:06}", (n + 1) % 1_000_000)
}

/// 用给定码完整走一遍配对，按协议顺序收集各步结果（遇到失败提前返回）。
fn attempt(
    agent: &mut AgentPairing,
    code: &str,
    agent_fp: &Fingerprint,
    ctrl_fp: &Fingerprint,
) -> Vec<PairStep> {
    let (mut ctrl, msg1) = ControllerPairing::start(code, ctrl_fp, agent_fp);

    let step1 = agent.on_message(&msg1, agent_fp, ctrl_fp);
    let msg2 = match &step1 {
        PairStep::Send(m) => m.clone(),
        _ => return vec![step1],
    };

    let step2 = ctrl.on_message(&msg2);
    let mac_c = match &step2 {
        PairStep::Send(m) => m.clone(),
        _ => return vec![step1, step2],
    };

    let step3 = agent.on_message(&mac_c, agent_fp, ctrl_fp);
    let mac_a = match &step3 {
        PairStep::Send(m) => m.clone(),
        _ => return vec![step1, step2, step3],
    };

    let step4 = ctrl.on_message(&mac_a);
    vec![step1, step2, step3, step4]
}

#[test]
fn correct_code_succeeds_and_is_one_time() {
    let mut agent = AgentPairing::new();
    let code = agent.new_code();
    assert_eq!(code.len(), 6);
    assert!(code.bytes().all(|b| b.is_ascii_digit()));

    let (_, expires) = agent.current_code().expect("new_code 后应有有效码");
    assert!(expires > Instant::now(), "有效期应为 5 分钟");

    let steps = attempt(&mut agent, &code, &fp(0xA1), &fp(0xC1));
    assert_eq!(steps.len(), 4, "完整协议 4 步");
    // 步骤 1：Agent 回 SPAKE2 消息（信封 2 字节 + 33 字节）
    assert!(
        matches!(&steps[0], PairStep::Send(m) if m.len() == 2 + 33),
        "第 1 步应为 SPAKE2 消息，实际 {steps:?}"
    );
    // 步骤 2：控制端回 HMAC 标签（信封 2 字节 + 32 字节）
    assert!(
        matches!(&steps[1], PairStep::Send(m) if m.len() == 2 + 32),
        "第 2 步应为控制端 MAC，实际 {steps:?}"
    );
    // 步骤 3：Agent 回 HMAC 标签
    assert!(
        matches!(&steps[2], PairStep::Send(m) if m.len() == 2 + 32),
        "第 3 步应为 Agent MAC，实际 {steps:?}"
    );
    assert!(
        matches!(steps[3], PairStep::Done),
        "第 4 步应 Done，实际 {steps:?}"
    );

    // 一次性：成功后码作废，同一码无法再次发起
    assert!(agent.current_code().is_none(), "成功后配对码应作废");
    let again = attempt(&mut agent, &code, &fp(0xA1), &fp(0xC1));
    assert!(matches!(again[0], PairStep::Failed(PairError::Expired)));
}

#[test]
fn wrong_code_fails_at_mac_round() {
    let mut agent = AgentPairing::new();
    let code = agent.new_code();

    // SPAKE2 对错码也会派生出消息与 key，错码在 HMAC 确认阶段才暴露
    let steps = attempt(&mut agent, &wrong_code(&code), &fp(0xA1), &fp(0xC1));
    assert!(matches!(steps[0], PairStep::Send(_)));
    assert!(matches!(steps[1], PairStep::Send(_)));
    assert!(
        matches!(steps[2], PairStep::Failed(PairError::WrongCode)),
        "Agent 应在第 3 步拒绝对端 MAC，实际 {steps:?}"
    );

    // 失败不消耗码（锁定另行处理）
    assert!(agent.current_code().is_some());
}

#[test]
fn expired_code_fails() {
    let mut agent = AgentPairing::new();
    let code = agent.new_code();
    agent.expire_code();
    assert!(
        agent.current_code().is_none(),
        "过期后 current_code 应为 None"
    );

    let steps = attempt(&mut agent, &code, &fp(0xA1), &fp(0xC1));
    assert!(
        matches!(steps[0], PairStep::Failed(PairError::Expired)),
        "过期码应直接拒绝，实际 {steps:?}"
    );
}

#[test]
fn five_failures_lock_ten_minutes() {
    let mut agent = AgentPairing::new();
    let code = agent.new_code();
    let wrong = wrong_code(&code);

    for i in 1..=5 {
        let steps = attempt(&mut agent, &wrong, &fp(0xA1), &fp(0xC1));
        assert!(
            matches!(steps[2], PairStep::Failed(PairError::WrongCode)),
            "第 {i} 次失败应为 WrongCode，实际 {steps:?}"
        );
    }

    // 第 6 次尝试：即使码正确也被锁定
    let steps = attempt(&mut agent, &code, &fp(0xA1), &fp(0xC1));
    match &steps[0] {
        PairStep::Failed(PairError::Locked { until }) => {
            assert!(*until > Instant::now(), "锁定截止应在未来（约 10 分钟后）");
        }
        other => panic!("第 6 次尝试应被锁定，实际 {other:?}"),
    }

    // 模拟锁定到期后解锁，同一码可继续完成配对
    agent.__force_unlock_for_tests();
    let steps = attempt(&mut agent, &code, &fp(0xA1), &fp(0xC1));
    assert!(
        matches!(steps[3], PairStep::Done),
        "解锁后应能成功，实际 {steps:?}"
    );
}

#[test]
fn fingerprint_substitution_mitm_fails() {
    // 场景：中间人同时终结两条 TLS 连接——
    // 控制端眼中对端证书 = 0x77（攻击者），Agent 眼中对端证书 = 0x99（攻击者）。
    // 双方 MAC 各自绑定「自己看到的指纹对」，视图不一致则必然有一侧拒绝。
    let mut agent = AgentPairing::new();
    let code = agent.new_code();
    let (mut ctrl, msg1) = ControllerPairing::start(&code, &fp(0xC1), &fp(0x77));
    let msg2 = match agent.on_message(&msg1, &fp(0xA1), &fp(0x99)) {
        PairStep::Send(m) => m,
        other => panic!("第 1 步应 Send，实际 {other:?}"),
    };
    let mac_c = match ctrl.on_message(&msg2) {
        PairStep::Send(m) => m,
        other => panic!("第 2 步应 Send，实际 {other:?}"),
    };

    // Agent 侧：mac_c 绑定 (C1, 0x77)，Agent 期望 (0x99, A1) —— 必须拒绝
    let step3 = agent.on_message(&mac_c, &fp(0xA1), &fp(0x99));
    assert!(
        matches!(step3, PairStep::Failed(PairError::WrongCode)),
        "Agent 必须拒绝绑定错误指纹的 MAC，实际 {step3:?}"
    );
    // 失败不消耗码
    assert!(agent.current_code().is_some());

    // 反向（控制端拒收）：正常视图完成前 3 步后篡改/伪造 Agent 的 MAC —— 必须拒绝
    let mut agent2 = AgentPairing::new();
    let code2 = agent2.new_code();
    let (mut ctrl2, msg1b) = ControllerPairing::start(&code2, &fp(0xC1), &fp(0xA1));
    let msg2b = match agent2.on_message(&msg1b, &fp(0xA1), &fp(0xC1)) {
        PairStep::Send(m) => m,
        other => panic!("第 1 步应 Send，实际 {other:?}"),
    };
    let mac_c2 = match ctrl2.on_message(&msg2b) {
        PairStep::Send(m) => m,
        other => panic!("第 2 步应 Send，实际 {other:?}"),
    };
    let mac_a = match agent2.on_message(&mac_c2, &fp(0xA1), &fp(0xC1)) {
        PairStep::Send(m) => m,
        other => panic!("第 3 步应 Send，实际 {other:?}"),
    };
    let mut tampered = mac_a.clone();
    tampered[2] ^= 0x01; // 翻转 MAC 首字节（模拟替换成攻击者的标签）
    assert!(
        matches!(
            ctrl2.on_message(&tampered),
            PairStep::Failed(PairError::WrongCode)
        ),
        "控制端必须拒绝被篡改的 Agent MAC"
    );

    // 真实指纹的配对不受中间人干扰，仍可完成
    let steps = attempt(&mut agent, &code, &fp(0xA1), &fp(0xC1));
    assert!(
        matches!(steps[3], PairStep::Done),
        "真实指纹应成功，实际 {steps:?}"
    );
}

#[test]
fn malformed_and_out_of_order_messages_are_protocol_errors() {
    let mut agent = AgentPairing::new();
    agent.new_code();

    // 垃圾消息 / 未知版本 / 未知 tag / 空 payload
    assert!(matches!(
        agent.on_message(b"garbage", &fp(0xA1), &fp(0xC1)),
        PairStep::Failed(PairError::Protocol)
    ));
    assert!(matches!(
        agent.on_message(&[9u8, 1, 1, 1], &fp(0xA1), &fp(0xC1)),
        PairStep::Failed(PairError::Protocol)
    ));
    assert!(matches!(
        agent.on_message(&[1u8, 9, 1, 1, 1], &fp(0xA1), &fp(0xC1)),
        PairStep::Failed(PairError::Protocol)
    ));
    assert!(matches!(
        agent.on_message(&[1u8, 1], &fp(0xA1), &fp(0xC1)),
        PairStep::Failed(PairError::Protocol)
    ));

    // 未发起握手就收到 MAC（乱序）；注意 Protocol 失败同样计入失败计数（此处第 5 次，触发锁定）
    let mac_envelope = {
        let mut m = vec![1u8, 2];
        m.extend_from_slice(&[0u8; 32]);
        m
    };
    assert!(matches!(
        agent.on_message(&mac_envelope, &fp(0xA1), &fp(0xC1)),
        PairStep::Failed(PairError::Protocol)
    ));

    // 控制端在错误阶段收到 MAC / 复用已结束的状态机（控制端无失败计数）
    let code = agent.current_code().expect("码仍有效").0;
    let (mut ctrl, _msg1) = ControllerPairing::start(&code, &fp(0xC1), &fp(0xA1));
    assert!(matches!(
        ctrl.on_message(&mac_envelope),
        PairStep::Failed(PairError::Protocol)
    ));
    assert!(matches!(
        ctrl.on_message(&[1u8, 1, 1]),
        PairStep::Failed(PairError::Protocol)
    ));
    // 复用已结束的状态机
    assert!(matches!(
        ctrl.on_message(&[1u8, 1, 1]),
        PairStep::Failed(PairError::Protocol)
    ));
}
