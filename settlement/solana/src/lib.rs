use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, Mint, Token, TokenAccount, TransferChecked},
};

declare_id!("2iXtA8oeZqUU5pofxK971TCEvFGfems2AcDRaZHKD2pQ");

#[program]
pub mod arrha_solana {
    use super::*;
    pub fn initialize(
        ctx: Context<Initialize>,
        max_per_run: u64,
        max_outstanding: u64,
    ) -> Result<()> {
        require!(
            max_per_run > 0 && max_outstanding >= max_per_run,
            Fault::Terms
        );
        let c = &mut ctx.accounts.config;
        c.mint = ctx.accounts.mint.key();
        c.decimals = ctx.accounts.mint.decimals;
        c.max_per_run = max_per_run;
        c.max_outstanding = max_outstanding;
        c.outstanding = 0;
        Ok(())
    }
    /// The payer signs the native transaction containing all terms, exact program/accounts,
    /// current blockhash and fee. No reusable EVM signature or delegated signing shortcut.
    pub fn fund(ctx: Context<Fund>, terms: Terms) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let payer = ctx.accounts.payer.key();
        require!(
            terms.run_id != [0; 32]
                && terms.work_hash != [0; 32]
                && terms.evidence_policy != [0; 32],
            Fault::Terms
        );
        require!(
            terms.provider != Pubkey::default()
                && terms.provider != payer
                && terms.arbitrator != Pubkey::default()
                && terms.arbitrator != payer
                && terms.arbitrator != terms.provider
                && terms.receipt_signer != Pubkey::default(),
            Fault::Terms
        );
        require!(
            terms.fund_by < terms.accept_by
                && terms.accept_by < terms.deliver_by
                && terms.deliver_by < terms.review_until
                && terms.review_until < terms.resolve_by
                && now <= terms.fund_by,
            Fault::Deadline
        );
        let c = &mut ctx.accounts.config;
        require!(
            terms.amount > 0 && terms.amount <= c.max_per_run,
            Fault::Terms
        );
        let next = c
            .outstanding
            .checked_add(terms.amount)
            .ok_or(Fault::Terms)?;
        require!(next <= c.max_outstanding, Fault::Terms);
        let run = &mut ctx.accounts.run;
        run.payer = payer;
        run.terms = terms;
        run.state = 1;
        run.evidence = [0; 32];
        run.bump = ctx.bumps.run;
        c.outstanding = next;
        token::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.source.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.payer.to_account_info(),
                },
            ),
            run.terms.amount,
            c.decimals,
        )?;
        emit!(Transition {
            run: run.key(),
            state: 1,
            evidence: [0; 32],
            provider_amount: 0,
            payer_amount: 0
        });
        Ok(())
    }
    pub fn revoke(_ctx: Context<Revoke>, _nonce: u64) -> Result<()> {
        Ok(())
    }
    pub fn accept(ctx: Context<Act>) -> Result<()> {
        let run = &mut ctx.accounts.run;
        require_keys_eq!(
            ctx.accounts.actor.key(),
            run.terms.provider,
            Fault::Authority
        );
        require!(run.state == 1, Fault::State);
        require!(
            Clock::get()?.unix_timestamp <= run.terms.accept_by,
            Fault::Deadline
        );
        run.state = 2;
        emit_change(run);
        Ok(())
    }
    pub fn deliver(ctx: Context<Deliver>, evidence: [u8; 32]) -> Result<()> {
        let run = &mut ctx.accounts.run;
        require_keys_eq!(
            ctx.accounts.provider.key(),
            run.terms.provider,
            Fault::Authority
        );
        require_keys_eq!(
            ctx.accounts.receipt_signer.key(),
            run.terms.receipt_signer,
            Fault::Authority
        );
        require!(run.state == 2, Fault::State);
        require!(
            Clock::get()?.unix_timestamp <= run.terms.deliver_by,
            Fault::Deadline
        );
        require!(evidence != [0; 32], Fault::Terms);
        run.evidence = evidence;
        run.state = 3;
        emit_change(run);
        Ok(())
    }
    pub fn dispute(ctx: Context<Act>) -> Result<()> {
        let run = &mut ctx.accounts.run;
        require_keys_eq!(ctx.accounts.actor.key(), run.payer, Fault::Authority);
        require!(run.state == 3, Fault::State);
        require!(
            Clock::get()?.unix_timestamp <= run.terms.review_until,
            Fault::Deadline
        );
        run.state = 4;
        emit_change(run);
        Ok(())
    }
    pub fn settle(ctx: Context<Payout>) -> Result<()> {
        let r = &ctx.accounts.run;
        require!(r.state == 3, Fault::State);
        require!(
            ctx.accounts.actor.key() == r.payer
                || Clock::get()?.unix_timestamp > r.terms.review_until,
            Fault::Authority
        );
        let amount = r.terms.amount;
        payout(ctx, 5, amount)
    }
    pub fn resolve(ctx: Context<Payout>, provider_amount: u64) -> Result<()> {
        let r = &ctx.accounts.run;
        require!(r.state == 4, Fault::State);
        require_keys_eq!(
            ctx.accounts.actor.key(),
            r.terms.arbitrator,
            Fault::Authority
        );
        require!(
            Clock::get()?.unix_timestamp <= r.terms.resolve_by,
            Fault::Deadline
        );
        require!(provider_amount <= r.terms.amount, Fault::Terms);
        payout(ctx, 7, provider_amount)
    }
    pub fn refund(ctx: Context<Payout>) -> Result<()> {
        let r = &ctx.accounts.run;
        let now = Clock::get()?.unix_timestamp;
        let cancel = r.state == 1 && ctx.accounts.actor.key() == r.payer;
        let timeout = (r.state == 1 && now > r.terms.accept_by)
            || (r.state == 2 && now > r.terms.deliver_by)
            || (r.state == 4 && now > r.terms.resolve_by);
        require!(cancel || timeout, Fault::State);
        payout(ctx, 6, 0)
    }
}

fn emit_change(r: &Account<Run>) {
    emit!(Transition {
        run: r.key(),
        state: r.state,
        evidence: r.evidence,
        provider_amount: 0,
        payer_amount: 0
    });
}
fn payout(ctx: Context<Payout>, state: u8, provider_amount: u64) -> Result<()> {
    let r = &mut ctx.accounts.run;
    let payer_amount = r
        .terms
        .amount
        .checked_sub(provider_amount)
        .ok_or(Fault::Terms)?;
    ctx.accounts.config.outstanding = ctx
        .accounts
        .config
        .outstanding
        .checked_sub(r.terms.amount)
        .ok_or(Fault::Terms)?;
    r.state = state;
    let bump = [r.bump];
    let seeds: &[&[u8]] = &[b"run", r.payer.as_ref(), &r.terms.run_id, &bump];
    let signers = &[seeds];
    for (amount, destination) in [
        (
            provider_amount,
            ctx.accounts.provider_tokens.to_account_info(),
        ),
        (payer_amount, ctx.accounts.payer_tokens.to_account_info()),
    ] {
        if amount > 0 {
            token::transfer_checked(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    TransferChecked {
                        from: ctx.accounts.vault.to_account_info(),
                        mint: ctx.accounts.mint.to_account_info(),
                        to: destination,
                        authority: r.to_account_info(),
                    },
                    signers,
                ),
                amount,
                ctx.accounts.config.decimals,
            )?;
        }
    }
    emit!(Transition {
        run: r.key(),
        state,
        evidence: r.evidence,
        provider_amount,
        payer_amount
    });
    Ok(())
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, InitSpace)]
pub struct Terms {
    pub run_id: [u8; 32],
    pub provider: Pubkey,
    pub arbitrator: Pubkey,
    pub receipt_signer: Pubkey,
    pub amount: u64,
    pub work_hash: [u8; 32],
    pub evidence_policy: [u8; 32],
    pub fund_by: i64,
    pub accept_by: i64,
    pub deliver_by: i64,
    pub review_until: i64,
    pub resolve_by: i64,
    pub nonce: u64,
}
#[account]
#[derive(InitSpace)]
pub struct Config {
    pub mint: Pubkey,
    pub decimals: u8,
    pub max_per_run: u64,
    pub max_outstanding: u64,
    pub outstanding: u64,
}
#[account]
#[derive(InitSpace)]
pub struct Run {
    pub payer: Pubkey,
    pub terms: Terms,
    pub state: u8,
    pub evidence: [u8; 32],
    pub bump: u8,
}
#[account]
#[derive(InitSpace)]
pub struct Nonce {}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(init, payer = authority, space = 8 + Config::INIT_SPACE, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    pub mint: Account<'info, Mint>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ Fault::Authority)]
    pub program: Program<'info, crate::program::ArrhaSolana>,
    #[account(constraint = program_data.upgrade_authority_address == Some(authority.key()) @ Fault::Authority)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
#[instruction(terms: Terms)]
pub struct Fund<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump, has_one = mint)]
    pub config: Account<'info, Config>,
    #[account(init, payer = payer, space = 8 + Run::INIT_SPACE, seeds = [b"run", payer.key().as_ref(), &terms.run_id], bump)]
    pub run: Account<'info, Run>,
    #[account(init, payer = payer, space = 8 + Nonce::INIT_SPACE, seeds = [b"nonce", payer.key().as_ref(), &terms.nonce.to_le_bytes()], bump)]
    pub nonce: Account<'info, Nonce>,
    pub mint: Account<'info, Mint>,
    #[account(mut, token::mint = mint, token::authority = payer)]
    pub source: Account<'info, TokenAccount>,
    #[account(init, payer = payer, associated_token::mint = mint, associated_token::authority = run)]
    pub vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
#[instruction(nonce: u64)]
pub struct Revoke<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    #[account(init, payer = payer, space = 8 + Nonce::INIT_SPACE, seeds = [b"nonce", payer.key().as_ref(), &nonce.to_le_bytes()], bump)]
    pub used_nonce: Account<'info, Nonce>,
    pub system_program: Program<'info, System>,
}
#[derive(Accounts)]
pub struct Act<'info> {
    pub actor: Signer<'info>,
    #[account(mut, seeds = [b"run", run.payer.as_ref(), &run.terms.run_id], bump = run.bump)]
    pub run: Account<'info, Run>,
}
#[derive(Accounts)]
pub struct Deliver<'info> {
    pub provider: Signer<'info>,
    pub receipt_signer: Signer<'info>,
    #[account(mut, seeds = [b"run", run.payer.as_ref(), &run.terms.run_id], bump = run.bump)]
    pub run: Account<'info, Run>,
}
#[derive(Accounts)]
pub struct Payout<'info> {
    pub actor: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump, has_one = mint)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"run", run.payer.as_ref(), &run.terms.run_id], bump = run.bump)]
    pub run: Account<'info, Run>,
    pub mint: Account<'info, Mint>,
    #[account(mut, associated_token::mint = mint, associated_token::authority = run)]
    pub vault: Account<'info, TokenAccount>,
    #[account(mut, token::mint = mint, constraint = payer_tokens.owner == run.payer @ Fault::Authority)]
    pub payer_tokens: Account<'info, TokenAccount>,
    #[account(mut, token::mint = mint, constraint = provider_tokens.owner == run.terms.provider @ Fault::Authority)]
    pub provider_tokens: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}
#[event]
pub struct Transition {
    pub run: Pubkey,
    pub state: u8,
    pub evidence: [u8; 32],
    pub provider_amount: u64,
    pub payer_amount: u64,
}
#[error_code]
pub enum Fault {
    #[msg("Invalid terms or budget")]
    Terms,
    #[msg("Unauthorized signer or destination")]
    Authority,
    #[msg("Invalid lifecycle state")]
    State,
    #[msg("Deadline invalid or expired")]
    Deadline,
}
