use anchor_lang::prelude::*;
use anchor_spl::token_interface::{self, Mint, TokenAccount, TokenInterface, TransferChecked};

// Placeholder: run `anchor keys sync` to replace it with your real program id.
declare_id!("23EsnF49ijwYb13TuK7sAHpfhepHdsoSznVhg7twxAZx");

const SECONDS_PER_YEAR: u128 = 31_536_000;
const SECONDS_PER_MONTH: i64 = 2_592_000; // 30 days
const AUCTION_SECONDS: i64 = 7 * 24 * 60 * 60; // 168 hours
const BPS: u128 = 10_000;

#[program]
pub mod sarahi {
    use super::*;

    /// Creates the global config. `agent` is the SARAHI agent key (approves loans)
    /// and `oracle` is the legal oracle key (verifies trusts).
    pub fn initialize_config(
        ctx: Context<InitializeConfig>,
        agent: Pubkey,
        oracle: Pubkey,
        max_ltv_bps: u16,
    ) -> Result<()> {
        require!(max_ltv_bps > 0 && max_ltv_bps <= 6_000, SarahiError::InvalidLtv);
        let c = &mut ctx.accounts.config;
        c.admin = ctx.accounts.admin.key();
        c.agent = agent;
        c.oracle = oracle;
        c.max_ltv_bps = max_ltv_bps;
        c.bump = ctx.bumps.config;
        Ok(())
    }

    /// The owner registers a property. `trust_hash` is the hash of the trust
    /// document and `appraisal` is the appraised value in USD with 6 decimals.
    pub fn register_property(
        ctx: Context<RegisterProperty>,
        property_id: u64,
        trust_hash: [u8; 32],
        appraisal: u64,
    ) -> Result<()> {
        require!(appraisal > 0, SarahiError::InvalidAmount);
        let p = &mut ctx.accounts.property;
        p.owner = ctx.accounts.owner.key();
        p.property_id = property_id;
        p.trust_hash = trust_hash;
        p.appraisal = appraisal;
        p.status = PropertyStatus::Registered;
        p.bump = ctx.bumps.property;
        Ok(())
    }

    /// The legal oracle confirms the deed is held in the trust
    /// and the appraisal is valid.
    pub fn verify_property(ctx: Context<VerifyProperty>) -> Result<()> {
        let p = &mut ctx.accounts.property;
        require!(p.status == PropertyStatus::Registered, SarahiError::BadPropertyState);
        p.status = PropertyStatus::Verified;
        Ok(())
    }

    /// A lender deposits liquidity into the pool vault (no pool shares yet).
    pub fn deposit_liquidity(ctx: Context<DepositLiquidity>, amount: u64) -> Result<()> {
        require!(amount > 0, SarahiError::InvalidAmount);
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.lp_token.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.lp.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )
    }

    /// Opens the loan. Requires the SARAHI agent signature (approval) and the
    /// borrower signature. The amount cannot exceed the max LTV of the appraisal.
    pub fn open_loan(
        ctx: Context<OpenLoan>,
        principal: u64,
        rate_bps: u16,
        term_months: u16,
    ) -> Result<()> {
        require!(principal > 0, SarahiError::InvalidAmount);
        require!((12..=60).contains(&term_months), SarahiError::InvalidTerm);

        let prop = &mut ctx.accounts.property;
        require!(prop.status == PropertyStatus::Verified, SarahiError::BadPropertyState);

        // principal / appraisal <= max_ltv_bps / 10_000
        let lhs = (principal as u128).checked_mul(BPS).ok_or(SarahiError::Overflow)?;
        let rhs = (prop.appraisal as u128)
            .checked_mul(ctx.accounts.config.max_ltv_bps as u128)
            .ok_or(SarahiError::Overflow)?;
        require!(lhs <= rhs, SarahiError::LtvExceeded);

        let now = Clock::get()?.unix_timestamp;
        let term_secs = (term_months as i64)
            .checked_mul(SECONDS_PER_MONTH)
            .ok_or(SarahiError::Overflow)?;

        let loan = &mut ctx.accounts.loan;
        loan.borrower = ctx.accounts.borrower.key();
        loan.property = prop.key();
        loan.mint = ctx.accounts.mint.key();
        loan.principal = principal;
        loan.rate_bps = rate_bps;
        loan.term_secs = term_secs;
        loan.start_ts = now;
        loan.maturity_ts = now.checked_add(term_secs).ok_or(SarahiError::Overflow)?;
        loan.auction_end_ts = 0;
        loan.status = LoanStatus::Active;
        loan.bump = ctx.bumps.loan;
        prop.status = PropertyStatus::Encumbered;

        let seeds: &[&[u8]] = &[b"pool", &[ctx.bumps.pool_authority]];
        token_interface::transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.borrower_token.to_account_info(),
                    authority: ctx.accounts.pool_authority.to_account_info(),
                },
                &[seeds],
            ),
            principal,
            ctx.accounts.mint.decimals,
        )?;

        emit!(LoanOpened { loan: loan.key(), borrower: loan.borrower, principal, rate_bps });
        Ok(())
    }

    /// Full repayment (principal + simple interest over the term). Releases the property.
    pub fn repay_loan(ctx: Context<RepayLoan>) -> Result<()> {
        let loan = &mut ctx.accounts.loan;
        require!(loan.status == LoanStatus::Active, SarahiError::BadLoanState);

        let due = total_due(loan)?;
        token_interface::transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                TransferChecked {
                    from: ctx.accounts.borrower_token.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.borrower.to_account_info(),
                },
            ),
            due,
            ctx.accounts.mint.decimals,
        )?;

        loan.status = LoanStatus::Repaid;
        ctx.accounts.property.status = PropertyStatus::Released;
        emit!(LoanRepaid { loan: loan.key(), amount: due });
        Ok(())
    }

    /// Anyone can mark a default if the loan matured unpaid.
    /// Opens the 168-hour auction window (the auction itself is on the roadmap).
    pub fn mark_default(ctx: Context<MarkDefault>) -> Result<()> {
        let loan = &mut ctx.accounts.loan;
        require!(loan.status == LoanStatus::Active, SarahiError::BadLoanState);
        let now = Clock::get()?.unix_timestamp;
        require!(now >= loan.maturity_ts, SarahiError::NotMatured);

        loan.status = LoanStatus::Defaulted;
        loan.auction_end_ts = now.checked_add(AUCTION_SECONDS).ok_or(SarahiError::Overflow)?;
        ctx.accounts.property.status = PropertyStatus::Foreclosing;
        emit!(AuctionOpened { loan: loan.key(), ends_at: loan.auction_end_ts });
        Ok(())
    }
}

fn total_due(loan: &Loan) -> Result<u64> {
    let interest = (loan.principal as u128)
        .checked_mul(loan.rate_bps as u128)
        .and_then(|v| v.checked_mul(loan.term_secs as u128))
        .ok_or(SarahiError::Overflow)?
        / (BPS * SECONDS_PER_YEAR);
    let total = (loan.principal as u128).checked_add(interest).ok_or(SarahiError::Overflow)?;
    u64::try_from(total).map_err(|_| error!(SarahiError::Overflow))
}

// ───────────────────────────── Accounts ─────────────────────────────

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub agent: Pubkey,
    pub oracle: Pubkey,
    pub max_ltv_bps: u16,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Property {
    pub owner: Pubkey,
    pub property_id: u64,
    pub trust_hash: [u8; 32],
    pub appraisal: u64,
    pub status: PropertyStatus,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Loan {
    pub borrower: Pubkey,
    pub property: Pubkey,
    pub mint: Pubkey,
    pub principal: u64,
    pub rate_bps: u16,
    pub term_secs: i64,
    pub start_ts: i64,
    pub maturity_ts: i64,
    pub auction_end_ts: i64,
    pub status: LoanStatus,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum PropertyStatus { Registered, Verified, Encumbered, Released, Foreclosing }

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum LoanStatus { Active, Repaid, Defaulted }

// ──────────────────────────── Contexts ────────────────────────────

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,
    #[account(init, payer = admin, space = 8 + Config::INIT_SPACE, seeds = [b"config"], bump)]
    pub config: Account<'info, Config>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(property_id: u64)]
pub struct RegisterProperty<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,
    #[account(
        init, payer = owner, space = 8 + Property::INIT_SPACE,
        seeds = [b"property", owner.key().as_ref(), &property_id.to_le_bytes()], bump
    )]
    pub property: Account<'info, Property>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct VerifyProperty<'info> {
    #[account(seeds = [b"config"], bump = config.bump, has_one = oracle)]
    pub config: Account<'info, Config>,
    pub oracle: Signer<'info>,
    #[account(mut)]
    pub property: Account<'info, Property>,
}

#[derive(Accounts)]
pub struct DepositLiquidity<'info> {
    pub lp: Signer<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(mut, token::mint = mint, token::authority = lp)]
    pub lp_token: InterfaceAccount<'info, TokenAccount>,
    /// CHECK: data-less PDA; it is only the vault authority.
    #[account(seeds = [b"pool"], bump)]
    pub pool_authority: UncheckedAccount<'info>,
    #[account(mut, token::mint = mint, token::authority = pool_authority)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct OpenLoan<'info> {
    #[account(seeds = [b"config"], bump = config.bump, has_one = agent)]
    pub config: Account<'info, Config>,
    pub agent: Signer<'info>,
    #[account(mut)]
    pub borrower: Signer<'info>,
    #[account(mut, constraint = property.owner == borrower.key() @ SarahiError::NotOwner)]
    pub property: Account<'info, Property>,
    #[account(
        init, payer = borrower, space = 8 + Loan::INIT_SPACE,
        seeds = [b"loan", property.key().as_ref()], bump
    )]
    pub loan: Account<'info, Loan>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: data-less PDA; it is only the vault authority.
    #[account(seeds = [b"pool"], bump)]
    pub pool_authority: UncheckedAccount<'info>,
    #[account(mut, token::mint = mint, token::authority = pool_authority)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = borrower)]
    pub borrower_token: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct RepayLoan<'info> {
    pub borrower: Signer<'info>,
    #[account(mut, has_one = borrower, has_one = property, has_one = mint)]
    pub loan: Account<'info, Loan>,
    #[account(mut)]
    pub property: Account<'info, Property>,
    pub mint: InterfaceAccount<'info, Mint>,
    /// CHECK: data-less PDA; it is only the vault authority.
    #[account(seeds = [b"pool"], bump)]
    pub pool_authority: UncheckedAccount<'info>,
    #[account(mut, token::mint = mint, token::authority = pool_authority)]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    #[account(mut, token::mint = mint, token::authority = borrower)]
    pub borrower_token: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
pub struct MarkDefault<'info> {
    pub caller: Signer<'info>,
    #[account(mut, has_one = property)]
    pub loan: Account<'info, Loan>,
    #[account(mut)]
    pub property: Account<'info, Property>,
}

// ───────────────────── Events and errors ─────────────────────

#[event]
pub struct LoanOpened { pub loan: Pubkey, pub borrower: Pubkey, pub principal: u64, pub rate_bps: u16 }
#[event]
pub struct LoanRepaid { pub loan: Pubkey, pub amount: u64 }
#[event]
pub struct AuctionOpened { pub loan: Pubkey, pub ends_at: i64 }

#[error_code]
pub enum SarahiError {
    #[msg("Invalid max LTV (must be above 0 and at most 60%)")] InvalidLtv,
    #[msg("Invalid amount")] InvalidAmount,
    #[msg("Term must be between 12 and 60 months")] InvalidTerm,
    #[msg("Property is not in the required state")] BadPropertyState,
    #[msg("Loan exceeds the max LTV of the appraisal")] LtvExceeded,
    #[msg("Loan is not in the required state")] BadLoanState,
    #[msg("Loan has not matured yet")] NotMatured,
    #[msg("Only the property owner can request the loan")] NotOwner,
    #[msg("Arithmetic overflow")] Overflow,
}
