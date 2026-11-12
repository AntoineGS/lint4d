unit PrivateOverride;

interface

type
  TBase = class
  protected
    procedure Dispatch; virtual;
  end;

  TChild = class(TBase)
  private
    procedure Dispatch; override;
  end;

implementation

procedure TBase.Dispatch;
begin
end;

procedure TChild.Dispatch;
begin
end;

end.
